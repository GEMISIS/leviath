//! One component per state field: how each part of a [`RunState`] is placed
//! on a run's entity.
//!
//! Each function here reads one field of the state (and the spec, where the
//! field's meaning depends on the graph) and returns the component that holds
//! it. Reading a live run back into a state is the same mapping the other way,
//! field by field.

use std::collections::HashMap;

use bevy_ecs::prelude::*;

use crate::components::{AgentMessage, AgentState, AgentStatus, ContextWindow, MessageInbox};
use crate::persistence::{FinalOutput, RunClock, RunMetadata, RunOutcomeFlags, TokenTotals};
use crate::pipeline::spec_view;
use crate::pipeline::{LastTransition, StageLedger, VisitCounts};
use crate::spec::launch::Unattended;
use crate::spec::names::StageName;
use crate::spec::run_spec::RunSpec;
use crate::state::{
    Clock, PipelinePhase, RunState, RunStatus, Spend, StageRecord, StageStatus, VisitRecord,
};

/// The position of the stage the run is in. A cursor naming no stage of the
/// graph (which a state taken from this graph never does) is read as the
/// first stage.
pub(crate) fn stage_index(spec: &RunSpec, state: &RunState) -> usize {
    spec_view::stage_index(&spec.graph, state.cursor.stage.as_str()).unwrap_or(0)
}

/// `status`.
pub(crate) fn agent_status(status: &RunStatus) -> AgentStatus {
    match status {
        RunStatus::Idle => AgentStatus::Idle,
        RunStatus::Active => AgentStatus::Active,
        RunStatus::Waiting => AgentStatus::Waiting,
        RunStatus::Paused => AgentStatus::Paused,
        RunStatus::Complete => AgentStatus::Complete,
        RunStatus::Error(message) => AgentStatus::Error {
            message: message.clone(),
        },
        RunStatus::Cancelled => AgentStatus::Cancelled,
    }
}

/// `status`, `cursor`, `accepts_messages` and `children`.
pub(crate) fn agent_state(spec: &RunSpec, state: &RunState) -> AgentState {
    AgentState {
        agent_id: spec.run_id.to_string(),
        current_stage: state.cursor.stage.to_string(),
        current_visit: state.cursor.visit.clone(),
        iteration: state.cursor.iteration as usize,
        status: agent_status(&state.status),
        spawned_children_ids: state.children.iter().map(ToString::to_string).collect(),
        pending_wait: None,
        accepts_messages: state.accepts_messages,
    }
}

/// `inbox`. Each message is addressed to this run.
pub(crate) fn message_inbox(spec: &RunSpec, state: &RunState) -> MessageInbox {
    MessageInbox {
        messages: state
            .inbox
            .iter()
            .map(|m| AgentMessage {
                agent_id: spec.run_id.to_string(),
                content: m.text.clone(),
                target_region: m.region.clone(),
                parts: Vec::new(),
            })
            .collect(),
    }
}

/// `visits`.
pub(crate) fn visit_counts(state: &RunState) -> VisitCounts {
    VisitCounts(
        state
            .visits
            .iter()
            .map(|(stage, n)| (stage.to_string(), *n as usize))
            .collect(),
    )
}

/// `progress`.
pub(crate) fn stage_progress(state: &RunState) -> crate::pipeline::StageProgress {
    let p = &state.progress;
    crate::pipeline::StageProgress {
        total_tool_calls: p.total_tool_calls as usize,
        text_only_nudges: p.text_only_nudges as usize,
        cut_off_nudges: p.cut_off_nudges as usize,
        raise_output_cap: p.raise_output_cap,
        iterations: p.iterations as usize,
        modifying_tool_calls: p.modifying_tool_calls as usize,
        blocked_modification_calls: p.blocked_modification_calls as usize,
        entry_region_digests: p
            .entry_region_digests
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect(),
        gate_reentries: p.gate_reentries as usize,
        stage_started_at: p.stage_started_at,
        waiting_since: p.waiting_since,
        edits_by_path: p
            .edits_by_path
            .iter()
            .map(|(k, v)| (k.clone(), *v as usize))
            .collect(),
        stuck_fired: p.stuck_fired,
        images_produced: p.images_produced as usize,
        no_image_nudges: p.no_image_nudges as usize,
    }
}

/// A stage the run has not entered yet, as the state records it.
pub(crate) fn pending_stage(stage: StageName) -> StageRecord {
    StageRecord {
        stage,
        status: StageStatus::Pending,
        entered: false,
        spend: Spend::default(),
        models: Vec::new(),
        visits: Vec::new(),
        region_tokens: Default::default(),
        first_call_prompt_tokens: None,
        runaway_warned: false,
        output_cap_raised: false,
        started_at: None,
        ended_at: None,
        clock: Clock::default(),
    }
}

fn stage_run_status(status: StageStatus) -> leviath_core::run_meta::StageRunStatus {
    use leviath_core::run_meta::StageRunStatus as S;
    match status {
        StageStatus::Pending => S::Pending,
        StageStatus::Active => S::Active,
        StageStatus::WaitingInput => S::WaitingInput,
        StageStatus::Complete => S::Complete,
        StageStatus::Error => S::Error,
        StageStatus::Skipped => S::Skipped,
    }
}

/// A cost is known exactly when no call was priced from published rates and
/// none went unpriced; it is known at all when none went unpriced.
fn cost(spend: &Spend) -> (Option<f64>, bool) {
    let known = spend.unpriced_calls == 0;
    (
        known.then_some(spend.priced_usd),
        known && spend.computed_calls == 0,
    )
}

/// A working clock, or none for one that has never run.
fn active_clock(clock: &Clock) -> Option<leviath_core::run_meta::ActiveClock> {
    (clock != &Clock::default()).then_some(leviath_core::run_meta::ActiveClock {
        banked_secs: clock.banked_secs,
        since: clock.since,
    })
}

fn visit_record(visit: &VisitRecord) -> leviath_core::run_meta::StageVisitRecord {
    let (cost_usd, cost_is_exact) = cost(&visit.spend);
    leviath_core::run_meta::StageVisitRecord {
        id: visit.id.clone(),
        entered_at: visit.entered_at,
        left_at: visit.left_at,
        prompt_tokens: visit.spend.prompt_tokens as usize,
        completion_tokens: visit.spend.completion_tokens as usize,
        cached_tokens: visit.spend.cached_tokens as usize,
        cache_write_tokens: visit.spend.cache_write_tokens as usize,
        cost_usd,
        unpriced_calls: visit.spend.unpriced_calls as usize,
        cost_is_exact,
        cost_priced_usd: visit.spend.priced_usd,
        active: active_clock(&visit.clock),
    }
}

/// `ledger`. A stage's visit count is the state's `visits` for it, which keeps
/// counting past the ledger's cap on recorded visits.
pub(crate) fn stage_ledger(state: &RunState) -> StageLedger {
    StageLedger(
        state
            .ledger
            .iter()
            .enumerate()
            .map(|(index, rec)| {
                let (cost_usd, cost_is_exact) = cost(&rec.spend);
                leviath_core::run_meta::StageRecord {
                    name: rec.stage.to_string(),
                    index,
                    status: stage_run_status(rec.status),
                    entered: rec.entered,
                    prompt_tokens: rec.spend.prompt_tokens as usize,
                    completion_tokens: rec.spend.completion_tokens as usize,
                    cached_tokens: rec.spend.cached_tokens as usize,
                    cache_write_tokens: rec.spend.cache_write_tokens as usize,
                    cost_usd,
                    unpriced_calls: rec.spend.unpriced_calls as usize,
                    cost_is_exact,
                    cost_priced_usd: rec.spend.priced_usd,
                    models: rec
                        .models
                        .iter()
                        .map(|m| leviath_core::run_meta::StageModelUse {
                            provider: m
                                .provider
                                .as_ref()
                                .map(ToString::to_string)
                                .unwrap_or_default(),
                            model: m.model.to_string(),
                        })
                        .collect(),
                    visits: rec.visits.iter().map(visit_record).collect(),
                    visit_count: state
                        .visits
                        .get(rec.stage.as_str())
                        .map_or(rec.visits.len(), |n| *n as usize),
                    region_tokens: rec
                        .region_tokens
                        .iter()
                        .map(|(k, v)| (k.clone(), *v as usize))
                        .collect(),
                    first_call_prompt_tokens: rec.first_call_prompt_tokens.map(|n| n as usize),
                    runaway_warned: rec.runaway_warned,
                    output_cap_raised: rec.output_cap_raised,
                    started_at: rec.started_at,
                    ended_at: rec.ended_at,
                    active: active_clock(&rec.clock),
                }
            })
            .collect(),
    )
}

/// `context`: each region shaped as the graph declares it (the current stage's
/// layout first, then the graph's, then any stage's), and the regions the
/// runtime adds shaped as it adds them. Each region's budget, entries and taint
/// are the state's.
pub(crate) fn context_window(spec: &RunSpec, state: &RunState) -> ContextWindow {
    let graph = &spec.graph;
    let current = graph.stage(state.cursor.stage.as_str());
    let declared = current
        .and_then(|s| s.layout.as_ref())
        .into_iter()
        .chain(std::iter::once(&graph.layout))
        .chain(graph.stages.iter().filter_map(|s| s.layout.as_ref()));
    let defs: Vec<&crate::spec::graph::RegionDef> =
        declared.flat_map(|l| l.regions.iter()).collect();
    let shape = |name: &str, max_tokens: usize| -> leviath_core::Region {
        match defs.iter().find(|d| d.name.as_str() == name) {
            Some(def) => {
                let def = spec_view::region_definition(def, max_tokens);
                let mut region = leviath_core::Region::new(def.name, def.kind, max_tokens);
                region.summarizable = def.summarizable;
                region.admission = def.admission;
                region.volatility = def.volatility;
                region.accepts = def.accepts;
                region.description = def.description;
                region.describe_in_prompt = def.describe_in_prompt;
                region
            }
            None => runtime_region(name, max_tokens),
        }
    };
    ContextWindow::from_state(&state.context, &shape)
}

/// A region the runtime adds to every window, shaped as it adds it: the
/// conversation, tool results, the final output and stage instructions.
fn runtime_region(name: &str, max_tokens: usize) -> leviath_core::Region {
    let kind = match name {
        "conversation" => leviath_core::RegionKind::SlidingWindow {
            max_items: 50,
            eviction_strategy: leviath_core::EvictionStrategy::PerItem,
        },
        "tool_results" => leviath_core::RegionKind::Temporary,
        _ => leviath_core::RegionKind::Pinned,
    };
    leviath_core::Region::new(name.to_string(), kind, max_tokens)
}

/// Spawn-time notes, on a new run: each stage's plan notes, tagged with the
/// stage's position, so they are the first lines of that stage's log.
pub(crate) fn io_buffer(spec: &RunSpec, state: &RunState) -> crate::pipeline::StageIoBuffer {
    let logs = match state.seq {
        0 => spec
            .graph
            .stages
            .iter()
            .enumerate()
            .flat_map(|(i, stage)| {
                spec.stage(stage.name.as_str())
                    .into_iter()
                    .flat_map(|p| p.notes.iter().cloned())
                    .map(move |line| (i, line))
            })
            .collect(),
        _ => Vec::new(),
    };
    crate::pipeline::StageIoBuffer {
        output: Vec::new(),
        logs,
    }
}

/// The run's record: who it is, where it runs, what it was asked for, and
/// (from `title`) what it is called.
pub(crate) fn run_metadata(spec: &RunSpec, state: &RunState) -> RunMetadata {
    let blueprint = spec.origin.blueprint_name().map(str::to_string);
    let digest = spec.origin.digest().map(ToString::to_string);
    let task = spec
        .seeded
        .get("task")
        .map(|s| s.text.clone())
        .unwrap_or_default();
    RunMetadata {
        run_id: spec.run_id.to_string(),
        agent_name: blueprint
            .or_else(|| spec.graph.title.clone())
            .unwrap_or_default(),
        agent_path: String::new(),
        task,
        model: spec
            .stages
            .first()
            .map(|p| format!("{}/{}", p.provider, p.model)),
        workdir: spec.placement.workdir.to_string_lossy().into_owned(),
        num_stages: spec.graph.stages.len(),
        started_at: spec.created_at,
        parent_run_id: spec.placement.parent.as_ref().map(ToString::to_string),
        metadata: spec
            .delivery
            .metadata
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        callback_url: spec.delivery.callback.as_ref().map(|c| c.url.to_string()),
        callback_secret: spec
            .delivery
            .callback
            .as_ref()
            .and_then(|c| c.secret.as_ref())
            .map(|s| s.expose().to_string()),
        title: state.title.clone(),
        blueprint_digest: digest,
        title_error: None,
        unattended: spec.launch.unattended != Unattended::Off,
        yolo_profile: match &spec.launch.unattended {
            Unattended::Profile(name) => Some(name.to_string()),
            _ => None,
        },
        read_paths: None,
        output_request: spec.requested_output.as_ref().map(spec_view::output_spec),
        model_override: spec.requested_model.as_ref().map(ToString::to_string),
    }
}

/// `totals`.
pub(crate) fn token_totals(state: &RunState) -> TokenTotals {
    let s = &state.totals.spend;
    TokenTotals {
        prompt_tokens: s.prompt_tokens as usize,
        completion_tokens: s.completion_tokens as usize,
        cached_tokens: s.cached_tokens as usize,
        cache_write_tokens: s.cache_write_tokens as usize,
        tool_calls: state.totals.tool_calls as usize,
        cost: leviath_providers::CostTotals {
            priced_usd: s.priced_usd,
            reported_calls: s.reported_calls as usize,
            computed_calls: s.computed_calls as usize,
            unpriced_calls: s.unpriced_calls as usize,
        },
    }
}

/// `clock`.
pub(crate) fn run_clock(state: &RunState) -> RunClock {
    RunClock(leviath_core::run_meta::ActiveClock {
        banked_secs: state.clock.banked_secs,
        since: state.clock.since,
    })
}

/// `flags`.
pub(crate) fn outcome_flags(state: &RunState) -> RunOutcomeFlags {
    let f = &state.flags;
    RunOutcomeFlags(leviath_core::run_meta::RunFlags {
        modified_files: f.modified_files.clone(),
        modified_file_count: f.modified_file_count as usize,
        empty_output: f.empty_output,
        no_output_tools: f.no_output_tools,
        searches_run: f.searches_run as usize,
        searches_empty: f.searches_empty as usize,
        max_iterations_hit: f.max_iterations_hit as usize,
        gates_forced: f.gates_forced as usize,
        required_regions_abandoned: f.required_regions_abandoned.clone(),
        workspace_lost: f.workspace_lost,
        produced_output: f.produced_output,
        output_forced: f.output_forced as usize,
        splits_degraded: f.splits_degraded as usize,
        broken_scripts: f.broken_scripts.clone(),
    })
}

/// What the spec alone decides: the current stage's routing, compaction, loop
/// detection, and the markers for input capture, self-approving checkpoints
/// and gate prompts, and tool re-scans.
pub(crate) fn spec_components(
    entity: &mut EntityWorldMut<'_>,
    spec: &RunSpec,
    setup: &crate::pipeline::StageSetup,
) {
    let graph = &spec.graph;
    if let Some(routing) = &setup.routing {
        entity.insert(crate::components::ToolResultRoutingComponent {
            routing: routing.clone(),
        });
    }
    if let Some(c) = &graph.compaction {
        entity.insert(crate::pipeline::CompactionSettings(
            leviath_core::CompactionConfig {
                provider: c
                    .model
                    .provider
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                model: c.model.model.to_string(),
                system_prompt: c.system_prompt.clone(),
                user_prompt_template: c.user_prompt_template.clone(),
                max_summary_tokens: c.max_summary_tokens as usize,
                temperature: c.temperature,
            },
        ));
    }
    if let Some(r) = &graph.repetition {
        entity.insert(
            crate::repetition::RepetitionDetector::from_detection_config(
                &crate::spec::blueprint::RepetitionDetectionConfig {
                    max_repeat_calls: r.max_repeat_calls.map(|n| n as usize),
                    max_readonly_streak: r.max_readonly_streak.map(|n| n as usize),
                    enabled: r.enabled,
                },
            ),
        );
    }
    if spec.launch.capture_model_input {
        entity.insert(crate::pipeline::CaptureModelInput);
    }
    if spec.auto_answers.checkpoints {
        entity.insert(crate::components::InteractionAutoApprove);
    }
    // Only a run under the taint gate has gate prompts to answer.
    if spec.auto_answers.gate && graph.taint_tracking == Some(true) {
        entity.insert(crate::components::GateAutoApprove);
    }
    use crate::spec::graph::ToolRescan;
    if graph.tool_rescan != ToolRescan::AtSpawn {
        entity.insert(crate::pipeline::DynamicTools);
    }
    if graph.tool_rescan == ToolRescan::BeforeDispatch {
        entity.insert(crate::pipeline::RescanBeforeDispatch);
    }
}

/// `title`: a run that has not been named yet asks for a title, when the host
/// bound the chain of models a title call may walk (which is how the operator
/// says titles are wanted at all). Only a top-level run that was given a task
/// and has done nothing yet: a child is listed under its parent, a run with no
/// task has nothing to be named after, and a resumed run already had its turn.
pub(crate) fn title_request(entity: &mut EntityWorldMut<'_>, spec: &RunSpec, state: &RunState) {
    let has_task = spec
        .seeded
        .get("task")
        .is_some_and(|s| !s.text.trim().is_empty());
    let wanted = entity.contains::<crate::title::TitleCandidates>()
        && state.seq == 0
        && state.title.is_none()
        && spec.placement.parent.is_none()
        && has_task;
    if wanted {
        entity.insert(crate::title::PendingTitle);
    }
}

/// `final_output` and `last_transition`, when the run has them.
pub(crate) fn optional_state(entity: &mut EntityWorldMut<'_>, state: &RunState) {
    if let Some(out) = &state.final_output {
        entity.insert(FinalOutput(leviath_core::output::FinalOutput {
            content: out.content.clone(),
            format: out.format.clone(),
            stage: out.stage.to_string(),
            submitted_at: out.submitted_at,
            truncated: out.truncated,
            artifacts: Vec::new(),
        }));
    }
    if let Some(t) = &state.last_transition {
        entity.insert(LastTransition(t.clone()));
    }
}

/// `phase` and `pending`: the marker that puts the run in front of the system
/// that drives it next.
///
/// A run waiting on a reply, a summary or a person goes back to asking (the
/// reply it waited on did not survive); a run with tool calls in flight has
/// them dispatched again, with the results that came back carried along; a
/// run choosing its next edge is asked again among the same edges.
pub(crate) fn phase(entity: &mut EntityWorldMut<'_>, spec: &RunSpec, state: &RunState) {
    use crate::pipeline::{AwaitingTransitionChoice, ReadyToInfer, WaitingForChildren};
    match &state.phase {
        PipelinePhase::Done | PipelinePhase::FanOut => {}
        PipelinePhase::WaitingForChildren => {
            entity.insert(WaitingForChildren);
        }
        PipelinePhase::AwaitingChoice(names) => {
            let edges = spec
                .graph
                .edges_from(state.cursor.stage.as_str())
                .filter(|e| names.contains(&e.name))
                .cloned()
                .collect();
            entity.insert(AwaitingTransitionChoice(edges));
        }

        _ => match &state.pending {
            Some(batch) => pending_batch(entity, batch),
            None => {
                entity.insert(ReadyToInfer);
            }
        },
    }
}

/// `pending`: the batch's calls as the model made them, the results already
/// in, and the marker that dispatches the rest.
pub(crate) fn pending_batch(entity: &mut EntityWorldMut<'_>, batch: &crate::state::PendingBatch) {
    let calls = batch
        .calls
        .iter()
        .map(|c| crate::components::ToolCall {
            tool_id: c.id.clone(),
            name: c.name.clone(),
            arguments: c.args.value().clone(),
            thought_signature: c.thought_signature.clone(),
        })
        .collect();
    let done = batch
        .done
        .iter()
        .map(|(id, r)| {
            let text = match r.is_error {
                true => format!("[error] {}", r.text),
                false => r.text.clone(),
            };
            (id.clone(), text.into())
        })
        .collect();
    entity.insert((
        crate::components::InferenceResult {
            attempt_id: String::new(),
            response: String::new(),
            tool_calls: calls,
            tokens_used: 0,
            cut_off_at: None,
            reasoning: None,
            parts: Vec::new(),
        },
        crate::pipeline::RecoveredResults(done),
        crate::pipeline::ReadyForTools,
    ));
}

/// The entities of the runs in the world, by run id, for the fan-out workers
/// a state names. Workers are placed before the run that started them when a
/// tree is resumed; one that is not here is counted as failed.
pub(crate) fn worker_entities(world: &mut World, state: &RunState) -> HashMap<String, Entity> {
    let Some(fan_out) = &state.fan_out else {
        return HashMap::new();
    };
    let wanted: Vec<String> = fan_out.active.iter().map(|(_, r)| r.to_string()).collect();
    let mut q = world.query::<(Entity, &AgentState)>();
    q.iter(world)
        .filter(|(_, s)| wanted.contains(&s.agent_id))
        .map(|(e, s)| (s.agent_id.clone(), e))
        .collect()
}

/// `fan_out`: the fan-out in progress, under the stage that started it.
pub(crate) fn fan_out(
    world: &mut World,
    entity: Entity,
    state: &RunState,
    workers: &HashMap<String, Entity>,
) {
    let Some(f) = &state.fan_out else {
        return;
    };
    let restored = crate::fanout::FanOutState {
        config: f.config.clone(),
        max_workers: f.max_workers as usize,
        pending: f
            .queued
            .iter()
            .map(|item| crate::fanout::WorkItem {
                id: item.id.clone(),
                inputs: item
                    .inputs
                    .0
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.to_raw()))
                    .collect(),
            })
            .collect(),
        active: f
            .active
            .iter()
            .map(|(item, run)| (item.clone(), run.to_string()))
            .collect(),
        summaries: f.done.clone(),
        failures: f.failed.clone(),
        parts: Vec::new(),
        paused: f.paused,
        origin: Default::default(),
    };
    crate::fanout::restore_fan_out_waiting(world, entity, restored, &|run| {
        workers.get(run).copied()
    });
}
