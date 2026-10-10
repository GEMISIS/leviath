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
    BlobFile, Clock, FanOutState, FileRef, FinalOutputState, Flags, OpenInteraction, PendingBatch,
    PipelinePhase, RunFiles, RunState, RunStatus, Spend, StageFile, StageProgress, StageRecord,
    StageStatus, ToolResultState, Totals, VisitRecord, WorkItemState,
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

/// What a record spent. One that names no cost at all and counts no call as
/// unpriced (a record from before runs priced their calls) has a cost that
/// is not known, so it shows no cost rather than a cost of nothing, and no
/// unpriced call, as every earlier release showed it.
fn spend(
    prompt: usize,
    completion: usize,
    cached: usize,
    cache_write: usize,
    (priced_usd, cost): (f64, Option<f64>),
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
        cost_unknown: cost.is_none() && unpriced == 0,
    }
}

/// A record's working clock, settled at `at`. A record from before the clock
/// existed has none, and worked for its wall-clock span, from `from` to
/// `at`, as every earlier release showed it.
fn clock(active: Option<leviath_core::run_meta::ActiveClock>, from: Option<i64>, at: i64) -> Clock {
    let mut c = active.unwrap_or_else(|| leviath_core::run_meta::ActiveClock {
        banked_secs: from.map_or(0, |from| leviath_core::duration::between(from, at)),
        since: None,
    });
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
            (meta.cost_priced_usd, meta.cost_usd),
            meta.cost_is_exact,
            meta.unpriced_calls,
        ),
        tool_calls: meta.tool_calls as u64,
    };
    state.clock = clock(meta.active, Some(meta.started_at), meta.updated_at);
    state.flags = flags(&meta.flags);
    state.children = meta
        .children
        .iter()
        .filter_map(|c| RunId::new(c.as_str()).ok())
        .collect();
    state.title = meta.title.clone();
    state.title_error = meta.title_error.clone();
    state.wait_reason = meta
        .waiting_on
        .as_ref()
        .map(leviath_runtime::state::WaitState::from);
}

/// The state the run was last in. What reading its window leaves out is
/// added to `losses`, and `named` is every stored part its steps named, in
/// the order they named them.
pub(crate) fn last(
    old: &LegacyRun,
    spec: &RunSpec,
    losses: &mut Losses,
    named: Vec<BlobFile>,
    report: &mut Report,
) -> RunState {
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
    let ctx = context_in(graph, stage, &old.folded.context, losses);
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
    // A run whose record names its stage was in it. With no ledger record of
    // the stage to count its visits by, that is one visit; a ledger record
    // counts them as the release that wrote it did, which for a release
    // that did not count them is none. One whose record names no stage
    // entered none, and is listed with none.
    if stage_of(graph, meta).is_some() && !old.stages.iter().any(|r| r.name == stage.name.as_str())
    {
        state.visits.entry(stage.name.clone()).or_insert(1);
    }
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
    let fan = old
        .fanout
        .as_ref()
        .map(|f| fan_out(f, stage, &old.dir, report));
    // A fan-out whose every worker never left a run behind merges nothing
    // when it resumes, and lists as one that came back empty, as the
    // release that wrote it listed it.
    if let Some((f, gone)) = &fan
        && *gone > 0
        && !matches!(
            state.status,
            RunStatus::Complete | RunStatus::Error(_) | RunStatus::Cancelled
        )
        && f.active.is_empty()
        && f.queued.is_empty()
        && f.done.is_empty()
    {
        state.flags.splits_degraded += 1;
    }
    state.fan_out = fan.map(|(f, _)| f);
    state.interactions = old
        .point
        .as_ref()
        .map(|p| interaction(p, stage, &spec.run_id, report))
        .into_iter()
        .collect();
    state.final_output = final_output(old, stage, report);
    state.files = files(old);
    state.blobs = blobs(old, named, &state.context);
    if let Some(why) = spec.origin.never_resumes()
        && !matches!(
            state.status,
            RunStatus::Complete | RunStatus::Error(_) | RunStatus::Cancelled
        )
    {
        report.note(crate::recorded::stopped(meta, why));
        end(
            &mut state,
            format!(
                "this run was converted from an earlier release with the graph it recorded rather than its blueprint, so it cannot resume: {why}"
            ),
        );
    }
    // Nothing answers a question a 0.1.0 worker asked: the worker is gone,
    // and 0.1.0 kept no record of the call that asked it to reopen it from.
    if let Some(question) = &old.question
        && state.status == RunStatus::Waiting
    {
        let why = format!(
            "it was waiting on {question}, which a Leviath 0.1.0 worker asked; that worker is gone and 0.1.0 kept no record of the call that asked it, so the question cannot be reopened"
        );
        report.note(crate::recorded::stopped(meta, &why));
        end(
            &mut state,
            format!("this run was converted from an earlier release and cannot resume: {why}"),
        );
    }
    // An old record files the stage a run was cancelled in as failed, and a
    // paused run's stage as running; converted, it reads as its run does.
    // Every other stage reads as its record says: a stage an old record
    // never reached is pending there, and every earlier release showed it so.
    state.settle_stage_here();
    interrupt_running_calls(&mut state, report);
    // Kept only when it says something the last step's time does not: a
    // worker's record is touched again when its parent reaps it.
    state.last_progress_at = old
        .listed
        .last_progress_at
        .filter(|at| *at < old.listed.updated_at);
    if old.listed.active.is_none() {
        // What every earlier release showed as its working time.
        state.clock = clock(None, Some(old.listed.started_at), old.listed.updated_at);
    }
    report_unrecorded(&state, report);
    state
}

/// Settle the calls the old daemon was running when it stopped. Stopping
/// killed each one part way, or it is still running on its own, so it is
/// never started again: it comes back interrupted, for the model to check,
/// as the release that wrote the run gave it back when it resumed.
fn interrupt_running_calls(state: &mut RunState, report: &mut Report) {
    let before: Vec<String> = state
        .pending
        .iter()
        .flat_map(|b| b.done.keys().cloned())
        .collect();
    leviath_runtime::restore::interrupt_in_flight(state);
    for id in state
        .pending
        .iter()
        .flat_map(|b| b.done.keys())
        .filter(|id| !before.contains(id))
    {
        report.note(format!(
            "tool call {id} was running when the old daemon stopped: it comes back interrupted and is not run again"
        ));
    }
}

/// End a run that cannot carry on, saying why.
fn end(state: &mut RunState, why: String) {
    state.status = RunStatus::Error(why);
    state.phase = PipelinePhase::Done;
    state.pending = None;
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
        StageRunStatus::Paused => StageStatus::Paused,
        StageRunStatus::Complete => StageStatus::Complete,
        StageRunStatus::Error => StageStatus::Error,
        StageRunStatus::Cancelled => StageStatus::Cancelled,
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
            (v.cost_priced_usd, v.cost_usd),
            v.cost_is_exact,
            v.unpriced_calls,
        ),
        clock: clock(v.active, Some(v.entered_at), v.left_at.unwrap_or(at)),
    }
}

/// Whether the run was ever in the stage. A record from before stages kept
/// the answer names none, so it is read off what the record says the stage
/// did: it spent, started, or got past pending. A visit alone does not say
/// so: a fan-out worker is placed in its graph's entry stage before it is
/// moved to the stage it works in, and every release recorded that as a
/// visit to a stage the worker never entered.
fn entered(r: &OldStage) -> bool {
    r.entered
        || r.prompt_tokens + r.completion_tokens > 0
        || r.started_at.is_some()
        || !matches!(r.status, StageRunStatus::Pending | StageRunStatus::Skipped)
}

fn ledger(r: &OldStage, meta: &RunMeta) -> Option<StageRecord> {
    let at = meta.updated_at;
    Some(StageRecord {
        stage: StageName::new(r.name.as_str()).ok()?,
        status: stage_status(&r.status),
        entered: entered(r),
        spend: spend(
            r.prompt_tokens,
            r.completion_tokens,
            r.cached_tokens,
            r.cache_write_tokens,
            (r.cost_priced_usd, r.cost_usd),
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
        clock: clock(r.active, r.started_at, r.ended_at.unwrap_or(at)),
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
    // Every call of the batch was recorded as dispatched, as the execution
    // the old journal named (none, before releases named one).
    let executions = b
        .calls
        .iter()
        .map(|c| (c.id.clone(), c.execution_id.clone()))
        .collect();
    PendingBatch {
        calls,
        done,
        executions,
        requested_by: b.requested_by.clone(),
        held: None,
    }
}

/// The fan-out the run was waiting on, and how many of its workers left no
/// run behind in the runs directory `dir` is in.
fn fan_out(
    f: &FanOutFile,
    stage: &StageDef,
    dir: &std::path::Path,
    report: &mut Report,
) -> (FanOutState, usize) {
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
    let mut failed = f.failures.clone();
    let mut active = Vec::new();
    // A worker the old run still waited on but that left no run directory
    // beside it never started, or its files are gone: nothing will ever
    // report for it, so it is a failure, named as one.
    let runs = dir.parent();
    for (item, run) in &f.active {
        match RunId::new(run.as_str()) {
            Ok(id) if runs.is_some_and(|r| r.join(run).is_dir()) => {
                active.push((item.clone(), id));
            }
            Ok(_) => {
                let why = format!(
                    "worker run {run} has no run directory: it never started, or its files are gone"
                );
                report.note(format!("fan-out work item {item:?} failed: {why}"));
                failed.push((item.clone(), why));
            }
            Err(e) => report.note(format!("fan-out worker {run:?} was left out: {e}")),
        }
    }
    let gone = failed.len() - f.failures.len();
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
    let state = FanOutState {
        stage: stage.name.clone(),
        config,
        max_workers: (f.max_workers < usize::MAX).then_some(n32(f.max_workers)),
        queued,
        active,
        done: f.summaries.clone(),
        failed,
        paused: f.paused,
        origin,
        parts: Vec::new(),
    };
    (state, gone)
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
    let bytes = match &old.final_output {
        Some(content) => content.len() as u64,
        None => {
            report.fill(
                "final_output.bytes",
                "0",
                "the run recorded a final output but its file is missing",
            );
            0
        }
    };
    Some(FinalOutputState {
        bytes,
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

/// The files the old run keeps beside its run file, as the run file names
/// them: the answer, and each stage's logs and taint audit, where they are.
fn files(old: &LegacyRun) -> RunFiles {
    let mut files = RunFiles {
        final_output: old
            .final_output
            .as_ref()
            .map(|content| FileRef::whole(leviath_core::FINAL_OUTPUT_FILE, content.as_bytes())),
        stages: Vec::new(),
    };
    let stages = old.dir.join(leviath_runtime::state::files::STAGES_DIR);
    let mut indexes: Vec<u32> = std::fs::read_dir(stages)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse().ok())
        .collect();
    indexes.sort_unstable();
    for index in indexes {
        for which in [StageFile::Output, StageFile::Logs, StageFile::TaintAudit] {
            let path = which.path(index);
            let named = match which {
                StageFile::TaintAudit => std::fs::read(old.dir.join(&path))
                    .ok()
                    .map(|bytes| FileRef::whole(path, &bytes)),
                _ => std::fs::metadata(old.dir.join(&path))
                    .ok()
                    .filter(std::fs::Metadata::is_file)
                    .map(|meta| FileRef::log(path, meta.len())),
            };
            if let Some(file) = named {
                files.set_stage_file(index, which, file);
            }
        }
    }
    files
}

/// The stored parts the old run keeps under `blobs/`: each one `named` by
/// its steps and each one its last context holds, with where it came from,
/// then any other there by digest.
fn blobs(
    old: &LegacyRun,
    mut named: Vec<BlobFile>,
    context: &leviath_runtime::state::ContextState,
) -> Vec<BlobFile> {
    leviath_runtime::state::files::note_blobs(&mut named, context);
    for (digest, size) in &old.blobs {
        if !named.iter().any(|b| b.digest == *digest) {
            named.push(BlobFile {
                digest: digest.clone(),
                mime_type: "application/octet-stream".to_string(),
                size: *size,
                name: None,
                region: None,
                tool: None,
            });
        }
    }
    named
}
