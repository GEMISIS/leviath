//! Reading a live run out of the world.
//!
//! [`inspect`] is the one function that turns a run's components into its
//! [`RunState`]. The persist system records what it returns, a live
//! inspection shows what it returns, and insertion places the same fields
//! back, so each field here has its mirror in `insert`.
//!
//! `seq` is left at 0: a live state has no step number until the persist
//! system records it, and the run file is what numbers steps.

use std::collections::BTreeMap;

use bevy_ecs::prelude::{Entity, World};
use leviath_core::JsonDoc;
use leviath_core::mime::Part;
use leviath_core::region::{EntryContent, Region, RegionEntry};

use super::context::{
    BlobState, ContextState, EntryKind, EntryMeta, EntryState, PartBody, PartState, RegionState,
    TaintState, ToolCallState,
};
use super::{
    Clock, Cursor, FanOutState, FinalOutputState, Flags, MessageState, OpenInteraction,
    PendingBatch, PipelinePhase, RunState, RunStatus, Spend, StageProgress, StageRecord,
    StageStatus, ToolResultState, Totals, TransitionRecord, VisitRecord, WorkItemState,
};
use crate::components::{AgentState, AgentStatus, ContextWindow};
use crate::spec::inputs::{InputValue, InputValues, RawInput};
use crate::spec::names::{
    Digest, EdgeName, InputName, ModelId, ModelRef, ProviderName, RegionName, RunId, StageName,
};

/// The live state of the run on `entity`, or `None` when the entity is not a
/// run (it has no [`AgentState`]) or its current stage has no valid name.
///
/// A value the state has no room for (a region or child whose name is not a
/// valid name) is left out rather than failing the whole read.
pub fn inspect(world: &World, entity: Entity) -> Option<RunState> {
    let agent = world.get::<AgentState>(entity)?;
    let stage = StageName::new(agent.current_stage.as_str()).ok()?;
    let window = world.get::<ContextWindow>(entity);
    Some(RunState {
        seq: 0,
        status: status_of(&agent.status),
        cursor: Cursor {
            stage: stage.clone(),
            visit: agent.current_visit.clone(),
            iteration: agent.iteration as u32,
        },
        phase: phase_of(world, entity, agent),
        accepts_messages: agent.accepts_messages,
        visits: visits_of(world, entity),
        progress: world
            .get::<crate::pipeline::StageProgress>(entity)
            .map(progress_of)
            .unwrap_or_default(),
        ledger: world
            .get::<crate::pipeline::StageLedger>(entity)
            .map(|l| l.0.iter().filter_map(stage_record_of).collect())
            .unwrap_or_default(),
        context: window.map(context_of).unwrap_or_default(),
        pending: pending_of(world, entity),
        fan_out: world
            .get::<crate::fanout::FanOutWaiting>(entity)
            .map(|w| fan_out_of(w, &stage)),
        inbox: world
            .get::<crate::components::MessageInbox>(entity)
            .map(|i| i.messages.iter().map(message_of).collect())
            .unwrap_or_default(),
        interactions: interactions_of(world, &agent.agent_id),
        totals: world
            .get::<crate::persistence::TokenTotals>(entity)
            .map(totals_of)
            .unwrap_or_default(),
        clock: world
            .get::<crate::persistence::RunClock>(entity)
            .map(|c| clock_of(Some(c.0)))
            .unwrap_or_default(),
        flags: flags_of(world, entity, agent),
        children: agent
            .spawned_children_ids
            .iter()
            .filter_map(|c| RunId::new(c.as_str()).ok())
            .collect(),
        title: world
            .get::<crate::persistence::RunMetadata>(entity)
            .and_then(|m| m.title.clone()),
        final_output: world
            .get::<crate::persistence::FinalOutput>(entity)
            .and_then(|o| final_output_of(&o.0)),
        wait_reason: wait_reason_of(world, entity, agent),
        last_transition: last_transition_of(world, entity),
    })
}

/// The edge the run last took, as every system that moves a run between
/// stages records it.
fn last_transition_of(world: &World, entity: Entity) -> Option<TransitionRecord> {
    world
        .get::<crate::pipeline::LastTransition>(entity)
        .map(|t| t.0.clone())
}

fn status_of(status: &AgentStatus) -> RunStatus {
    match status {
        AgentStatus::Idle => RunStatus::Idle,
        AgentStatus::Active => RunStatus::Active,
        AgentStatus::Waiting => RunStatus::Waiting,
        AgentStatus::Paused => RunStatus::Paused,
        AgentStatus::Complete => RunStatus::Complete,
        AgentStatus::Error { message } => RunStatus::Error(message.clone()),
        AgentStatus::Cancelled => RunStatus::Cancelled,
    }
}

/// What the pipeline is doing, from the markers on the entity.
///
/// A run carries one phase marker at a time, but the waits sit on top of
/// them: a tool batch whose tool is asking a person still carries
/// `AwaitingTools`, and what it is actually waiting on is the person. So the
/// order below is the order a person would ask "why is this not moving".
fn phase_of(world: &World, entity: Entity, agent: &AgentState) -> PipelinePhase {
    use crate::pipeline as p;
    if p::is_terminal_status(&agent.status) {
        return PipelinePhase::Done;
    }
    if agent.status == AgentStatus::Paused || world.get::<p::PausedForSetup>(entity).is_some() {
        return PipelinePhase::Paused;
    }
    if let Some(w) = world.get::<p::Wedged>(entity) {
        return PipelinePhase::Wedged(format!("nothing has driven it since {}", w.since));
    }
    if let Some(s) = world
        .get::<p::DispatchStall>(entity)
        .filter(|s| s.reason != p::StallReason::PoolFull)
    {
        return PipelinePhase::Wedged(s.reason.label().to_string());
    }
    if asking_a_person(world, entity) {
        return PipelinePhase::AwaitingPerson;
    }
    if world.get::<crate::fanout::FanOutWaiting>(entity).is_some() {
        return PipelinePhase::FanOut;
    }
    if world.get::<p::WaitingForChildren>(entity).is_some() {
        return PipelinePhase::WaitingForChildren;
    }
    if let Some(choice) = world.get::<p::AwaitingTransitionChoice>(entity) {
        return PipelinePhase::AwaitingChoice(edge_names(&choice.0));
    }
    if world.get::<p::AwaitingCompaction>(entity).is_some() {
        return PipelinePhase::AwaitingCompaction;
    }
    if world.get::<p::AwaitingTools>(entity).is_some() {
        return PipelinePhase::AwaitingTools;
    }
    if world.get::<p::AwaitingInference>(entity).is_some() {
        return PipelinePhase::AwaitingInference;
    }
    PipelinePhase::ReadyToInfer
}

/// Whether the run is held on a question to a person: a prompt from a tool,
/// a taint gate, or a stage-boundary checkpoint.
fn asking_a_person(world: &World, entity: Entity) -> bool {
    world
        .get::<crate::components::AwaitingInteraction>(entity)
        .is_some()
        || world
            .get::<crate::gate_prompt::AwaitingGatePrompt>(entity)
            .is_some_and(|g| g.0 > 0)
        || world
            .get::<crate::interaction_points::AwaitingInteractionPoint>(entity)
            .is_some()
}

/// The names of the edges a stage is choosing between, as the graph names
/// them. An edge is matched by its target among the stage's own edges.
/// The edges the model is choosing between, by their names in the graph.
fn edge_names(edges: &[crate::spec::graph::EdgeDef]) -> Vec<EdgeName> {
    edges.iter().map(|e| e.name.clone()).collect()
}

fn visits_of(world: &World, entity: Entity) -> BTreeMap<StageName, u32> {
    world
        .get::<crate::pipeline::VisitCounts>(entity)
        .map(|v| {
            v.0.iter()
                .filter_map(|(s, n)| Some((StageName::new(s.as_str()).ok()?, *n as u32)))
                .collect()
        })
        .unwrap_or_default()
}

fn progress_of(p: &crate::pipeline::StageProgress) -> StageProgress {
    let counts = |m: &std::collections::HashMap<String, usize>| {
        m.iter().map(|(k, v)| (k.clone(), *v as u32)).collect()
    };
    StageProgress {
        total_tool_calls: p.total_tool_calls as u32,
        text_only_nudges: p.text_only_nudges as u32,
        cut_off_nudges: p.cut_off_nudges as u32,
        raise_output_cap: p.raise_output_cap,
        iterations: p.iterations as u32,
        modifying_tool_calls: p.modifying_tool_calls as u32,
        blocked_modification_calls: p.blocked_modification_calls as u32,
        entry_region_digests: p
            .entry_region_digests
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect(),
        gate_reentries: p.gate_reentries as u32,
        stage_started_at: p.stage_started_at,
        waiting_since: p.waiting_since,
        edits_by_path: counts(&p.edits_by_path),
        stuck_fired: p.stuck_fired,
        images_produced: p.images_produced as u32,
        no_image_nudges: p.no_image_nudges as u32,
    }
}

fn clock_of(active: Option<leviath_core::run_meta::ActiveClock>) -> Clock {
    active.map_or(Clock::default(), |a| Clock {
        banked_secs: a.banked_secs,
        since: a.since,
    })
}

/// A spend from the ledger's figures. The ledger keeps whether its cost is
/// exact rather than how each call was priced, so an inexact record reads as
/// one call priced from published rates.
fn spend_of(tokens: [usize; 4], priced_usd: f64, unpriced_calls: usize, exact: bool) -> Spend {
    Spend {
        prompt_tokens: tokens[0] as u64,
        completion_tokens: tokens[1] as u64,
        cached_tokens: tokens[2] as u64,
        cache_write_tokens: tokens[3] as u64,
        priced_usd,
        reported_calls: 0,
        computed_calls: u32::from(!exact),
        unpriced_calls: unpriced_calls as u32,
    }
}

fn stage_record_of(r: &leviath_core::run_meta::StageRecord) -> Option<StageRecord> {
    use leviath_core::run_meta::StageRunStatus as S;
    Some(StageRecord {
        stage: StageName::new(r.name.as_str()).ok()?,
        status: match r.status {
            S::Pending => StageStatus::Pending,
            S::Active => StageStatus::Active,
            S::WaitingInput => StageStatus::WaitingInput,
            S::Complete => StageStatus::Complete,
            S::Error => StageStatus::Error,
            S::Skipped => StageStatus::Skipped,
        },
        entered: r.entered,
        spend: spend_of(
            [
                r.prompt_tokens,
                r.completion_tokens,
                r.cached_tokens,
                r.cache_write_tokens,
            ],
            r.cost_priced_usd,
            r.unpriced_calls,
            r.cost_is_exact,
        ),
        models: r
            .models
            .iter()
            .filter_map(|m| {
                Some(ModelRef {
                    provider: ProviderName::new(m.provider.as_str()).ok(),
                    model: ModelId::new(m.model.as_str()).ok()?,
                })
            })
            .collect(),
        visits: r
            .visits
            .iter()
            .map(|v| VisitRecord {
                id: v.id.clone(),
                entered_at: v.entered_at,
                left_at: v.left_at,
                spend: spend_of(
                    [
                        v.prompt_tokens,
                        v.completion_tokens,
                        v.cached_tokens,
                        v.cache_write_tokens,
                    ],
                    v.cost_priced_usd,
                    v.unpriced_calls,
                    v.cost_is_exact,
                ),
                clock: clock_of(v.active),
            })
            .collect(),
        region_tokens: r
            .region_tokens
            .iter()
            .map(|(k, v)| (k.clone(), *v as u64))
            .collect(),
        first_call_prompt_tokens: r.first_call_prompt_tokens.map(|t| t as u64),
        runaway_warned: r.runaway_warned,
        output_cap_raised: r.output_cap_raised,
        started_at: r.started_at,
        ended_at: r.ended_at,
        clock: clock_of(r.active),
    })
}

/// The context window, as the run file stores it.
pub fn context_of(window: &ContextWindow) -> ContextState {
    let mut hidden: Vec<RegionName> = window
        .hidden
        .iter()
        .filter_map(|h| RegionName::new(h.as_str()).ok())
        .collect();
    hidden.sort();
    ContextState {
        regions: window.regions.iter().filter_map(region_of).collect(),
        hidden,
        max_tokens: window.max_tokens as u32,
    }
}

fn region_of(region: &Region) -> Option<RegionState> {
    Some(RegionState {
        name: RegionName::new(region.name.as_str()).ok()?,
        max_tokens: region.max_tokens as u32,
        current_tokens: region.current_tokens as u32,
        needs_message_compaction: region.needs_message_compaction,
        taint: region.taint.as_ref().map(|t| TaintState {
            level: t.level(),
            entries: (0..t.entry_count())
                .filter_map(|i| t.entry_taint(i))
                .collect(),
        }),
        entries: region.content.iter().map(entry_of).collect(),
    })
}

/// One region entry, as the run file stores it. See [`content_of`] for the
/// way back.
pub fn entry_of(entry: &RegionEntry) -> EntryState {
    let text_only = is_plain_text(entry.content.parts());
    EntryState {
        text: entry.content.as_str().to_string(),
        parts: match text_only {
            true => Vec::new(),
            false => entry.content.parts().iter().filter_map(part_of).collect(),
        },
        tokens: entry.tokens as u32,
        timestamp: entry.timestamp,
        kind: match &entry.kind {
            leviath_core::region::EntryKind::Text => EntryKind::Text,
            leviath_core::region::EntryKind::UserMessage => EntryKind::UserMessage,
            leviath_core::region::EntryKind::AssistantTurn { tool_calls } => {
                EntryKind::AssistantTurn(tool_calls.iter().map(tool_call_of).collect())
            }
            leviath_core::region::EntryKind::ToolResult {
                tool_call_id,
                tool_name,
                is_error,
            } => EntryKind::ToolResult {
                call_id: tool_call_id.clone(),
                tool: tool_name.clone(),
                is_error: *is_error,
            },
        },
        meta: entry
            .as_checklist_item()
            .map_or(EntryMeta::None, |item| EntryMeta::ChecklistItem {
                id: item.id as u32,
                done: item.done,
                note: item.note,
            }),
        key: entry.key.clone(),
        reasoning: entry.reasoning.clone(),
    }
}

/// The content an [`EntryState`] holds: its parts when it has any, and its
/// text as one plain part when it has none. The inverse of [`entry_of`].
pub fn content_of(entry: &EntryState) -> EntryContent {
    match entry.parts.is_empty() {
        true => EntryContent::text(entry.text.clone()),
        false => EntryContent::from_parts(entry.parts.iter().filter_map(part_from).collect()),
    }
}

/// Whether content is one plain text part and nothing else, which is how
/// [`EntryContent::text`] builds it and how [`content_of`] rebuilds it.
fn is_plain_text(parts: &[Part]) -> bool {
    match parts {
        [p] => {
            p.mime_type.as_str() == "text/plain"
                && p.name.is_none()
                && p.deliver.is_none()
                && !p.is_stored()
        }
        _ => false,
    }
}

fn tool_call_of(c: &leviath_core::region::SerializedToolCall) -> ToolCallState {
    ToolCallState {
        id: c.id.clone(),
        name: c.name.clone(),
        args: JsonDoc::new(c.arguments.clone()),
        thought_signature: c.thought_signature.clone(),
    }
}

fn part_of(part: &Part) -> Option<PartState> {
    Some(PartState {
        mime_type: part.mime_type.as_str().to_string(),
        body: match &part.body {
            leviath_core::mime::PartBody::Inline(s) => PartBody::Inline(s.clone()),
            leviath_core::mime::PartBody::Stored(b) => PartBody::Stored(BlobState {
                digest: Digest::new(b.sha256.as_str()).ok()?,
                size: b.size,
                width: b.width,
                height: b.height,
                duration_ms: b.duration_ms,
                tokens: b.tokens as u32,
                stand_in: b.stand_in.clone(),
            }),
        },
        name: part.name.clone(),
        deliver: part.deliver,
    })
}

fn part_from(part: &PartState) -> Option<Part> {
    let mime_type = leviath_core::mime::MimeType::parse(part.mime_type.as_str()).ok()?;
    let body = match &part.body {
        PartBody::Inline(s) => leviath_core::mime::PartBody::Inline(s.clone()),
        PartBody::Stored(b) => leviath_core::mime::PartBody::Stored(leviath_core::mime::BlobRef {
            sha256: b.digest.as_str().to_string(),
            mime_type: mime_type.clone(),
            size: b.size,
            width: b.width,
            height: b.height,
            duration_ms: b.duration_ms,
            tokens: b.tokens as usize,
            stand_in: b.stand_in.clone(),
        }),
    };
    Some(Part {
        mime_type,
        body,
        name: part.name.clone(),
        deliver: part.deliver,
    })
}

/// The tool batch in flight: the calls of the reply being run, and the
/// results already in for them.
///
/// The calls come from the reply itself, not the context window: the turn
/// that made them is written to the window only once every result is in.
fn pending_of(world: &World, entity: Entity) -> Option<PendingBatch> {
    world.get::<crate::pipeline::AwaitingTools>(entity)?;
    let calls: Vec<ToolCallState> = world
        .get::<crate::components::InferenceResult>(entity)?
        .tool_calls
        .iter()
        .map(|c| ToolCallState {
            id: c.tool_id.clone(),
            name: c.name.clone(),
            args: JsonDoc::new(c.arguments.clone()),
            thought_signature: c.thought_signature.clone(),
        })
        .collect();
    let mut done = BTreeMap::new();
    for (id, text) in world
        .get::<crate::pipeline::ContextToolResults>(entity)
        .map(|r| r.0.clone())
        .unwrap_or_default()
    {
        done.insert(id, result_of(text));
    }
    for (id, content) in world
        .get::<crate::pipeline::RecoveredResults>(entity)
        .map(|r| r.0.clone())
        .unwrap_or_default()
    {
        done.insert(id, result_of(content.into_string()));
    }
    Some(PendingBatch { calls, done })
}

fn result_of(text: String) -> ToolResultState {
    ToolResultState {
        is_error: text.starts_with("[error]"),
        text,
    }
}

fn fan_out_of(waiting: &crate::fanout::FanOutWaiting, stage: &StageName) -> FanOutState {
    let s = waiting.to_state();
    FanOutState {
        stage: stage.clone(),
        config: s.config.clone(),
        max_workers: s.max_workers as u32,
        queued: s
            .pending
            .into_iter()
            .map(|item| WorkItemState {
                id: item.id,
                inputs: inputs_of(item.inputs),
            })
            .collect(),
        active: s
            .active
            .into_iter()
            .filter_map(|(item, run)| Some((item, RunId::new(run).ok()?)))
            .collect(),
        done: s.summaries,
        failed: s.failures,
        paused: s.paused,
    }
}

/// A work item's free-form context as typed inputs: each key of an object is
/// an input, and anything else is one input named `context`.
/// A queued item's inputs, read by the shape they arrived in. A queued item
/// is checked against its worker's declarations when it starts, so here each
/// value keeps the type the wire gave it, and a name that is not a valid
/// input name is left out.
fn inputs_of(raw: std::collections::BTreeMap<String, RawInput>) -> InputValues {
    InputValues(
        raw.into_iter()
            .filter_map(|(k, v)| Some((InputName::new(k).ok()?, input_of(v))))
            .collect(),
    )
}

fn input_of(value: RawInput) -> InputValue {
    match value {
        RawInput::Bool(b) => InputValue::Bool(b),
        RawInput::Int(i) => InputValue::Int(i),
        RawInput::Float(x) => InputValue::Float(x),
        RawInput::Text(s) => InputValue::Text(s),
        RawInput::List(items) => InputValue::List(items.into_iter().map(input_of).collect()),
        RawInput::Record(map) => InputValue::Record(inputs_of(map).0),
    }
}

fn message_of(m: &crate::components::AgentMessage) -> MessageState {
    MessageState {
        // The inbox keeps who a message is for, not who sent it.
        from: String::new(),
        text: m.content.clone(),
        region: m.target_region.clone(),
    }
}

fn interactions_of(world: &World, agent_id: &str) -> Vec<OpenInteraction> {
    let Some(hub) = world.get_resource::<crate::interaction_hub::InteractionHub>() else {
        return Vec::new();
    };
    let mut open: Vec<OpenInteraction> = hub
        .pending()
        .into_iter()
        .filter(|(id, _)| id == agent_id)
        .map(|(_, r)| OpenInteraction {
            id: r.id,
            prompt: r.prompt,
            options: r.options,
        })
        .collect();
    open.sort_by(|a, b| a.id.cmp(&b.id));
    open
}

fn totals_of(t: &crate::persistence::TokenTotals) -> Totals {
    Totals {
        spend: Spend {
            prompt_tokens: t.prompt_tokens as u64,
            completion_tokens: t.completion_tokens as u64,
            cached_tokens: t.cached_tokens as u64,
            cache_write_tokens: t.cache_write_tokens as u64,
            priced_usd: t.cost.priced_usd,
            reported_calls: t.cost.reported_calls as u32,
            computed_calls: t.cost.computed_calls as u32,
            unpriced_calls: t.cost.unpriced_calls as u32,
        },
        tool_calls: t.tool_calls as u64,
    }
}

fn flags_of(world: &World, entity: Entity, agent: &AgentState) -> Flags {
    let mut flags = world
        .get::<crate::persistence::RunOutcomeFlags>(entity)
        .map(|f| f.0.clone())
        .unwrap_or_default();
    if let Some(v) = world.get::<crate::components::OutputValidators>(entity) {
        flags.broken_scripts = v.broken_names();
    }
    flags.produced_output = world
        .get::<crate::persistence::FinalOutput>(entity)
        .is_some();
    flags.empty_output = crate::persistence::is_empty_output(&agent.status, &flags);
    Flags {
        modified_files: flags.modified_files,
        modified_file_count: flags.modified_file_count as u32,
        empty_output: flags.empty_output,
        no_output_tools: flags.no_output_tools,
        searches_run: flags.searches_run as u32,
        searches_empty: flags.searches_empty as u32,
        max_iterations_hit: flags.max_iterations_hit as u32,
        gates_forced: flags.gates_forced as u32,
        required_regions_abandoned: flags.required_regions_abandoned,
        workspace_lost: flags.workspace_lost,
        produced_output: flags.produced_output,
        output_forced: flags.output_forced as u32,
        splits_degraded: flags.splits_degraded as u32,
        broken_scripts: flags.broken_scripts,
    }
}

fn final_output_of(o: &leviath_core::output::FinalOutput) -> Option<FinalOutputState> {
    Some(FinalOutputState {
        content: o.content.clone(),
        format: o.format.clone(),
        stage: StageName::new(o.stage.as_str()).ok()?,
        submitted_at: o.submitted_at,
        truncated: o.truncated,
    })
}

/// Why the run is parked, in the words `lev ps` uses, while it is.
fn wait_reason_of(world: &World, entity: Entity, agent: &AgentState) -> Option<String> {
    let parked = matches!(agent.status, AgentStatus::Waiting | AgentStatus::Paused);
    let children = world
        .get::<crate::components::SubAgentChildren>(entity)
        .map_or(0, |c| c.children.len());
    let markers = leviath_core::run_meta::WaitMarkers {
        gate_prompt: world
            .get::<crate::gate_prompt::AwaitingGatePrompt>(entity)
            .is_some_and(|g| g.0 > 0),
        interaction_point: world
            .get::<crate::interaction_points::AwaitingInteractionPoint>(entity)
            .is_some(),
        fan_out_outstanding: world
            .get::<crate::fanout::FanOutWaiting>(entity)
            .map(|f| f.outstanding()),
        children_outstanding: world
            .get::<crate::pipeline::WaitingForChildren>(entity)
            .map(|_| children),
        interaction: world
            .get_resource::<crate::interaction_hub::InteractionHub>()
            .and_then(|h| {
                h.pending()
                    .into_iter()
                    .find(|(id, _)| *id == agent.agent_id)
                    .map(|(_, r)| r.kind)
            }),
        awaiting_interaction: world
            .get::<crate::components::AwaitingInteraction>(entity)
            .is_some(),
        needs_setup: world
            .get::<crate::pipeline::PausedForSetup>(entity)
            .map(|p| leviath_core::run_meta::SetupNeeded {
                blocker: p.blocker,
                remedy: p.remedy.clone(),
            }),
    };
    leviath_core::run_meta::wait_reason_from(parked, &markers).map(|r| r.to_string())
}

#[cfg(test)]
#[path = "inspect_tests.rs"]
mod tests;
