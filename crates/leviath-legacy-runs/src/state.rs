//! The run's state, rebuilt from its journal, `stages.json`, `fanout.json`
//! and `interactions.json`.

use std::collections::BTreeMap;

use crate::journal::PendingToolBatch;
use leviath_core::JsonDoc;
use leviath_core::run_meta::{
    RunFlags, RunMeta, RunStatus as OldStatus, StageRecord as OldStage, StageRunStatus,
    StageVisitRecord, WaitReason,
};
use leviath_runtime::spec::graph::{FanOutDef, RunGraph, StageDef, StageMode};
use leviath_runtime::spec::inputs::{InputValue, InputValues};
use leviath_runtime::spec::names::{InputName, RunId, StageName};
use leviath_runtime::spec::run_spec::RunSpec;
use leviath_runtime::state::context::ToolCallState;
use leviath_runtime::state::{
    Clock, FanOutState, FinalOutputState, Flags, OpenInteraction, PendingBatch, PipelinePhase,
    RunState, RunStatus, Spend, StageProgress, StageRecord, StageStatus, ToolResultState, Totals,
    VisitRecord, WorkItemState,
};

use crate::context::{self, Losses, n32};
use crate::legacy::{FanOutFile, LegacyRun, PointFile};
use crate::report::Report;

/// The stage a run is in: the one its metadata names, when the graph has it.
pub(crate) fn stage_of<'g>(graph: &'g RunGraph, meta: &RunMeta) -> Option<&'g StageDef> {
    graph.stage(&meta.current_stage)
}

/// The stage a run starts in.
pub(crate) fn entry(graph: &RunGraph) -> &StageDef {
    graph
        .entry_stage()
        .expect("a graph read from a blueprint has at least one stage")
}

/// The context of `snapshot`, as the run file holds it while the run is in
/// `stage`.
pub(crate) fn context_in(
    graph: &RunGraph,
    stage: &StageDef,
    snapshot: &leviath_core::run_meta::ContextSnapshot,
    losses: &mut Losses,
) -> leviath_runtime::state::ContextState {
    let taint = graph.taint_tracking == Some(true) || stage.taint_tracking == Some(true);
    context::state(snapshot, stage.hide.clone(), taint, losses)
}

fn status(meta: &RunMeta) -> RunStatus {
    match meta.status {
        OldStatus::Starting => RunStatus::Idle,
        OldStatus::Running => RunStatus::Active,
        OldStatus::WaitingInput => RunStatus::Waiting,
        OldStatus::Complete | OldStatus::CompleteInteractive => RunStatus::Complete,
        OldStatus::Paused => RunStatus::Paused,
        OldStatus::Error => RunStatus::Error(
            meta.error
                .clone()
                .unwrap_or_else(|| "the run failed and did not say why".into()),
        ),
        OldStatus::Cancelled => RunStatus::Cancelled,
    }
}

fn phase(meta: &RunMeta, tools_in_flight: bool) -> PipelinePhase {
    match &meta.status {
        OldStatus::Starting | OldStatus::Running => match tools_in_flight {
            true => PipelinePhase::AwaitingTools,
            false => PipelinePhase::ReadyToInfer,
        },
        OldStatus::WaitingInput => match &meta.waiting_on {
            Some(WaitReason::FanOutWorkers { .. }) => PipelinePhase::FanOut,
            Some(WaitReason::Children { .. }) => PipelinePhase::WaitingForChildren,
            _ => PipelinePhase::AwaitingPerson,
        },
        OldStatus::Paused => PipelinePhase::Paused,
        OldStatus::Complete
        | OldStatus::CompleteInteractive
        | OldStatus::Error
        | OldStatus::Cancelled => PipelinePhase::Done,
    }
}

fn spend(
    prompt: usize,
    completion: usize,
    cached: usize,
    cache_write: usize,
    priced_usd: f64,
    exact: bool,
    unpriced: usize,
) -> Spend {
    Spend {
        prompt_tokens: prompt as u64,
        completion_tokens: completion as u64,
        cached_tokens: cached as u64,
        cache_write_tokens: cache_write as u64,
        priced_usd,
        reported_calls: 0,
        computed_calls: u32::from(!exact),
        unpriced_calls: n32(unpriced),
    }
}

fn clock(active: Option<leviath_core::run_meta::ActiveClock>, at: i64) -> Clock {
    let mut c = active.unwrap_or_default();
    c.settle(at);
    Clock {
        banked_secs: c.banked_secs,
        since: None,
    }
}

fn flags(f: &RunFlags) -> Flags {
    Flags {
        modified_files: f.modified_files.clone(),
        modified_file_count: n32(f.modified_file_count),
        empty_output: f.empty_output,
        no_output_tools: f.no_output_tools,
        searches_run: n32(f.searches_run),
        searches_empty: n32(f.searches_empty),
        max_iterations_hit: n32(f.max_iterations_hit),
        gates_forced: n32(f.gates_forced),
        required_regions_abandoned: f.required_regions_abandoned.clone(),
        workspace_lost: f.workspace_lost,
        produced_output: f.produced_output,
        output_forced: n32(f.output_forced),
        splits_degraded: n32(f.splits_degraded),
        broken_scripts: f.broken_scripts.clone(),
    }
}

/// Set every field of `state` that the run's metadata decides. Used for each
/// step of the journal as well as the last state, so the two agree.
pub(crate) fn from_meta(state: &mut RunState, meta: &RunMeta, graph: &RunGraph) {
    state.status = status(meta);
    state.phase = phase(meta, state.pending.is_some());
    if let Some(stage) = stage_of(graph, meta) {
        if stage.name != state.cursor.stage {
            state.cursor.visit.clear();
        }
        state.cursor.stage = stage.name.clone();
        state.accepts_messages = stage.accepts_messages;
    }
    state.cursor.iteration = n32(meta.iteration);
    state.totals = Totals {
        spend: spend(
            meta.prompt_tokens,
            meta.completion_tokens,
            meta.cached_tokens,
            meta.cache_write_tokens,
            meta.cost_priced_usd,
            meta.cost_is_exact,
            meta.unpriced_calls,
        ),
        tool_calls: meta.tool_calls as u64,
    };
    state.clock = clock(meta.active, meta.updated_at);
    state.flags = flags(&meta.flags);
    state.children = meta
        .children
        .iter()
        .filter_map(|c| RunId::new(c.as_str()).ok())
        .collect();
    state.title = meta.title.clone();
    state.wait_reason = meta
        .waiting_on
        .as_ref()
        .map(leviath_runtime::state::WaitState::from);
}

/// The state the run was last in.
pub(crate) fn last(old: &LegacyRun, spec: &RunSpec, report: &mut Report) -> RunState {
    let meta = old.meta();
    let graph = &spec.graph;
    let stage = stage_of(graph, meta).unwrap_or_else(|| {
        report.fill(
            "cursor.stage",
            entry(graph).name.as_str(),
            format!(
                "the run's stage {:?} is not in its graph",
                meta.current_stage
            ),
        );
        entry(graph)
    });
    let mut losses = Losses::default();
    let ctx = context_in(graph, stage, &old.folded.context, &mut losses);
    losses.report(report);
    let mut state = RunState::initial(stage.name.clone(), ctx, stage.accepts_messages);
    state.pending = old
        .folded
        .pending_batch
        .as_ref()
        .map(|b| pending(b, report));
    from_meta(&mut state, meta, graph);
    for c in meta
        .children
        .iter()
        .filter(|c| RunId::new(c.as_str()).is_err())
    {
        report.note(format!("child run {c:?} was left out: not a valid run id"));
    }
    state.ledger = old.stages.iter().filter_map(|r| ledger(r, meta)).collect();
    state.visits = visits(&old.stages);
    state.cursor.visit = old
        .stages
        .iter()
        .find(|r| r.name == stage.name.as_str())
        .and_then(|r| r.visits.last())
        .map(|v| v.id.clone())
        .unwrap_or_default();
    state.progress = StageProgress {
        iterations: n32(meta.iteration),
        stage_started_at: state
            .ledger
            .iter()
            .find(|r| r.stage == stage.name)
            .and_then(|r| r.visits.last())
            .map(|v| v.entered_at),
        ..StageProgress::default()
    };
    state.fan_out = old.fanout.as_ref().map(|f| fan_out(f, stage, report));
    state.interactions = old
        .point
        .as_ref()
        .map(|p| interaction(p, stage, &spec.run_id, report))
        .into_iter()
        .collect();
    state.final_output = final_output(old, stage, report);
    report_unrecorded(&state, report);
    state
}

/// Name the fields no old run records at all.
fn report_unrecorded(state: &RunState, report: &mut Report) {
    report.fill(
        "progress",
        "zero, but for iterations and stage_started_at",
        "an old run did not record its stage counters",
    );
    report.fill(
        "totals.spend.reported_calls",
        state.totals.spend.reported_calls,
        "an old run recorded only whether its cost was exact, not how each call was priced",
    );
    report.fill(
        "inbox",
        "[]",
        "an old run did not record messages still waiting to be delivered",
    );
    report.fill(
        "last_transition",
        "None",
        "an old run did not record which edge it took",
    );
    report.fill(
        "context.entries.timestamp",
        0,
        "an old context snapshot did not record when each entry was written",
    );
    report.fill(
        "context.regions.needs_message_compaction",
        "false",
        "an old context snapshot did not record it",
    );
}

fn stage_status(s: &StageRunStatus) -> StageStatus {
    match s {
        StageRunStatus::Pending => StageStatus::Pending,
        StageRunStatus::Active => StageStatus::Active,
        StageRunStatus::WaitingInput => StageStatus::WaitingInput,
        StageRunStatus::Complete => StageStatus::Complete,
        StageRunStatus::Error => StageStatus::Error,
        StageRunStatus::Skipped => StageStatus::Skipped,
    }
}

fn visit(v: &StageVisitRecord, at: i64) -> VisitRecord {
    VisitRecord {
        id: v.id.clone(),
        entered_at: v.entered_at,
        left_at: v.left_at,
        spend: spend(
            v.prompt_tokens,
            v.completion_tokens,
            v.cached_tokens,
            v.cache_write_tokens,
            v.cost_priced_usd,
            v.cost_is_exact,
            v.unpriced_calls,
        ),
        clock: clock(v.active, at),
    }
}

fn ledger(r: &OldStage, meta: &RunMeta) -> Option<StageRecord> {
    let at = meta.updated_at;
    Some(StageRecord {
        stage: StageName::new(r.name.as_str()).ok()?,
        status: stage_status(&r.status),
        entered: r.entered,
        spend: spend(
            r.prompt_tokens,
            r.completion_tokens,
            r.cached_tokens,
            r.cache_write_tokens,
            r.cost_priced_usd,
            r.cost_is_exact,
            r.unpriced_calls,
        ),
        models: r
            .models
            .iter()
            .filter_map(crate::plan::ledger_model)
            .collect(),
        visits: r.visits.iter().map(|v| visit(v, at)).collect(),
        region_tokens: r
            .region_tokens
            .iter()
            .map(|(k, v)| (k.clone(), *v as u64))
            .collect(),
        first_call_prompt_tokens: r.first_call_prompt_tokens.map(|n| n as u64),
        runaway_warned: r.runaway_warned,
        output_cap_raised: r.output_cap_raised,
        started_at: r.started_at,
        ended_at: r.ended_at,
        clock: clock(r.active, at),
    })
}

/// How many times each stage was entered, from its ledger record.
fn visits(stages: &[OldStage]) -> BTreeMap<StageName, u32> {
    stages
        .iter()
        .filter_map(|r| {
            let n = r.visit_count.max(r.visits.len());
            Some((StageName::new(r.name.as_str()).ok()?, n32(n))).filter(|(_, n)| *n > 0)
        })
        .collect()
}

fn pending(b: &PendingToolBatch, report: &mut Report) -> PendingBatch {
    let calls = b
        .calls
        .iter()
        .map(|c| ToolCallState {
            id: c.id.clone(),
            name: c.name.clone(),
            args: JsonDoc::parse(&c.arguments)
                .unwrap_or_else(|_| JsonDoc::new(serde_json::Value::String(c.arguments.clone()))),
            thought_signature: c.thought_signature.clone(),
        })
        .collect();
    let done: BTreeMap<String, ToolResultState> = b
        .calls
        .iter()
        .filter_map(|c| {
            let text = c.result.as_ref()?.as_str().to_string();
            Some((
                c.id.clone(),
                ToolResultState {
                    text,
                    is_error: false,
                },
            ))
        })
        .collect();
    if !done.is_empty() {
        report.fill(
            "pending.done.*.is_error",
            "false",
            "an old journal kept a finished call's text but not whether it failed",
        );
    }
    PendingBatch { calls, done }
}

fn fan_out(f: &FanOutFile, stage: &StageDef, report: &mut Report) -> FanOutState {
    let task = InputName::new("task").expect("`task` is an input name");
    let queued = f
        .pending
        .iter()
        .map(|item| WorkItemState {
            id: item.id.clone(),
            inputs: InputValues(BTreeMap::from([(
                task.clone(),
                InputValue::Text(format!(
                    "Work item id: {}\nContext: {}",
                    item.id, item.context
                )),
            )])),
        })
        .collect();
    if !f.pending.is_empty() {
        report.fill(
            "fan_out.queued.*.inputs",
            "{ task = \"Work item id: ...\\nContext: ...\" }",
            "an old work item was a JSON context, and a worker received it as this task text",
        );
    }
    let active = f
        .active
        .iter()
        .filter_map(|(item, run)| match RunId::new(run.as_str()) {
            Ok(id) => Some((item.clone(), id)),
            Err(e) => {
                report.note(format!("fan-out worker {run:?} was left out: {e}"));
                None
            }
        })
        .collect();
    // An old fan-out with no origin was a stage's, which is the default; an
    // origin is otherwise the same shape the run file keeps.
    let origin = serde_json::from_value(f.origin.clone()).unwrap_or_default();
    if !f.parts.is_empty() {
        report.note(format!(
            "{} files the fan-out's workers handed back are not kept",
            f.parts.len()
        ));
    }
    let config = match &stage.mode {
        StageMode::FanOut(def) => def.clone(),
        _ => {
            report.note(format!(
                "the fan-out a `fan_out` call started in stage \"{}\" resumes with that stage as its worker: an old run did not keep the call's settings",
                stage.name
            ));
            FanOutDef::same_graph(stage.name.clone())
        }
    };
    FanOutState {
        stage: stage.name.clone(),
        config,
        max_workers: n32(f.max_workers),
        queued,
        active,
        done: f.summaries.clone(),
        failed: f.failures.clone(),
        paused: f.paused,
        origin,
        parts: Vec::new(),
    }
}

fn interaction(
    p: &PointFile,
    stage: &StageDef,
    run_id: &RunId,
    report: &mut Report,
) -> OpenInteraction {
    let point = match &stage.mode {
        StageMode::InteractivePoints(points) => points.get(p.cursor),
        _ => None,
    };
    let id = format!("{run_id}-point-{}-{}", p.cursor, p.round);
    report.fill(
        "interactions[0].id",
        &id,
        "an old run kept the open interaction point but not its request id",
    );
    if !p.body.is_empty() {
        report.note("the document the open interaction point shows is the one in its region");
    }
    match point {
        Some(point) => OpenInteraction {
            id,
            prompt: point.prompt.clone(),
            options: point.options.clone(),
        },
        None => {
            report.fill(
                "interactions[0].prompt",
                "\"\"",
                format!(
                    "the stage {} has no interaction point {}",
                    stage.name, p.cursor
                ),
            );
            OpenInteraction {
                id,
                prompt: String::new(),
                options: Vec::new(),
            }
        }
    }
}

fn final_output(
    old: &LegacyRun,
    stage: &StageDef,
    report: &mut Report,
) -> Option<FinalOutputState> {
    let d = old.meta().final_output.as_ref()?;
    let content = old.final_output.clone().unwrap_or_else(|| {
        report.fill(
            "final_output.content",
            "\"\"",
            "the run recorded a final output but its file is missing",
        );
        String::new()
    });
    Some(FinalOutputState {
        content,
        format: d.format.clone(),
        stage: StageName::new(d.stage.as_str()).unwrap_or_else(|_| stage.name.clone()),
        submitted_at: d.submitted_at,
        truncated: d.truncated,
        artifacts: d
            .artifacts
            .iter()
            .map(|a| leviath_runtime::state::journal::ArtifactState {
                name: a.name.clone(),
                path: a.path.clone(),
                mime_type: a.mime_type.as_str().to_string(),
                size: a.size,
                sha256: a.sha256.clone(),
            })
            .collect(),
    })
}
