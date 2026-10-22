//! Continuing a finished run with a new message.
//!
//! A run that has finished (complete, failed or cancelled) can be taken up
//! again: it goes back to the stage its graph names in `continue_at` (the
//! entry stage by default), with the message as the start of a new segment.
//! Each region of its window keeps, clears, summarizes or reseeds what it
//! held, by its `on_continue` (or its kind, when it sets none).
//!
//! The work splits the way a resume's does. Off the world, [`prepare`]
//! checks the run can be continued, keeps the answer of the segment that
//! ended beside the run file, and reopens the state; the host reseeds and
//! binds it there too. On the world, [`place`] puts the run back, and the
//! host then has `apply` carry the regions over, enter the stage, leave the
//! message waiting and record the step that reopened it. The message is
//! delivered once the summaries the continuation asked for are in (see
//! `pipeline::deliver_continuation`), so a summary never swallows it.
//!
//! A fork is the same continuation on a copy: [`fork`] writes a new run whose
//! first state is the finished run's own, and the new run is continued. The
//! run it was copied from is never written to.

use std::collections::BTreeMap;
use std::path::Path;

use bevy_ecs::prelude::*;
use leviath_core::mime::InboundPart;
use tokio::sync::oneshot;

use crate::components::{AgentMessage, ContextWindow, FROM_CONTINUATION, MessageInbox};
use crate::persistence_bridge::Appended;
use crate::pipeline::{JournalSender, PendingEdgeCompact};
use crate::restore::Resumable;
use crate::spec::graph::{OnContinue, RunGraph, Seed};
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::names::{RegionName, RunId};
use crate::spec::run_spec::{RunSpec, SeededContent};
use crate::state::files::FileRef;
use crate::state::segment::{ContinuedState, SegmentEnd, kept_answer_path};
use crate::state::{PipelinePhase, RunState, RunStatus};

/// The message that continues a run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContinueMessage {
    /// What it says.
    pub text: String,
    /// The region it goes to. `None` is the conversation.
    pub region: Option<String>,
    /// Files it carries, landing beside its text.
    pub parts: Vec<InboundPart>,
}

/// A run reopened off the world, ready to bind and place.
#[derive(Debug)]
pub struct Prepared {
    /// The run, its state reopened at the stage it ended in.
    pub run: Resumable,
    /// What each region that reseeds holds again, by name. Filled by the
    /// host, which runs seeds.
    pub reseeded: BTreeMap<String, SeededContent>,
    continued: ContinuedState,
    message: ContinueMessage,
}

impl Prepared {
    /// The segment the continuation starts.
    pub fn segment(&self) -> u32 {
        self.continued.segment
    }
}

/// What reopening a run decided, carried on its entity from [`place`] to
/// [`apply`].
#[derive(Component, Debug)]
pub(crate) struct Reopened {
    spec: std::sync::Arc<RunSpec>,
    continued: ContinuedState,
    reseeded: BTreeMap<String, SeededContent>,
    message: ContinueMessage,
}

/// A refusal of a continuation: `message`, at `at`, for the reason `code`.
pub fn refused(code: IssueCode, at: SpecPath, message: impl Into<String>) -> SpawnIssues {
    SpawnIssue::new(at, code, message).into()
}

/// What a refused continuation says, for a caller that shows text: each
/// reason's sentence, in order.
pub fn refusal_text(issues: &SpawnIssues) -> String {
    issues
        .iter()
        .map(|issue| issue.message.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

/// Whether the run `spec` describes, standing in `state`, can be continued:
/// it has finished, it is not a sub-agent of another run, and it is not one
/// converted from an old release without its blueprint.
pub fn check(spec: &RunSpec, state: &RunState) -> Result<(), SpawnIssues> {
    let run = &spec.run_id;
    if let Some(parent) = &spec.placement.parent {
        return Err(refused(
            IssueCode::NotAllowed,
            SpecPath::root(),
            format!(
                "run '{run}' is a sub-agent of run '{parent}'; continue the run that started it"
            ),
        ));
    }
    if let Some(why) = spec.origin.never_resumes() {
        return Err(refused(
            IssueCode::NotAllowed,
            SpecPath::root(),
            format!("run '{run}' cannot be continued: {why}"),
        ));
    }
    match &state.status {
        RunStatus::Complete | RunStatus::Error(_) | RunStatus::Cancelled => Ok(()),
        status => Err(refused(
            IssueCode::Conflict,
            SpecPath::root(),
            format!(
                "run '{run}' has not finished ({}); send it a message, or wait for it to finish",
                status_word(status)
            ),
        )),
    }
}

/// How a status reads in a refusal.
fn status_word(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::Idle => "starting",
        RunStatus::Waiting => "waiting",
        RunStatus::Paused => "paused",
        _ => "running",
    }
}

/// The regions a continuation of a run of `graph` reseeds, each with the seed
/// it is filled from: every region whose `on_continue` is `reseed`, from the
/// first layout declaring it.
pub fn reseeding(graph: &RunGraph) -> Vec<(RegionName, Seed)> {
    let mut out: Vec<(RegionName, Seed)> = Vec::new();
    let layouts =
        std::iter::once(&graph.layout).chain(graph.stages.iter().filter_map(|s| s.layout.as_ref()));
    for region in layouts.flat_map(|layout| &layout.regions) {
        let reseeds = graph.continue_policy(region.name.as_str()) == Some(OnContinue::Reseed);
        if let (true, Some(seed)) = (reseeds, &region.seed)
            && out.iter().all(|(name, _)| *name != region.name)
        {
            out.push((region.name.clone(), seed.clone()));
        }
    }
    out
}

/// Run the seed of every region a continuation of `run` reseeds, against
/// `env`, as the run was resolved: its inputs, its code, its launch policy and
/// its workdir. A seed that fails leaves its region empty and says so in the
/// log; a command seed on a run that may not run commands is skipped.
pub async fn reseed(
    run: &Resumable,
    env: &dyn crate::spec::env::ResolveEnv,
) -> BTreeMap<String, SeededContent> {
    let spec = &run.spec;
    let manifest = spec.origin.manifest();
    let blueprint_dir = match manifest.is_empty() {
        true => None,
        false => Path::new(&manifest).parent(),
    };
    let mut out = BTreeMap::new();
    for (region, seed) in reseeding(&spec.graph) {
        let cx = crate::spec::env::SeedCx {
            run_id: &spec.run_id,
            agent: spec.origin.blueprint_name(),
            graph: &spec.graph,
            launch: &spec.launch,
            workdir: &spec.placement.workdir,
            blueprint_dir,
            commands_allowed: spec.launch.seed_commands,
            code: &run.code,
            code_refs: &spec.code,
            inputs: &spec.inputs,
        };
        let seeded = match (&seed, spec.launch.seed_commands) {
            (Seed::Command(_), false) => Err("command seeds are off for this run".to_string()),
            _ => env.seed(&seed, cx).await,
        };
        match seeded {
            Ok(content) => {
                out.insert(region.to_string(), content);
            }
            Err(why) => {
                let (run_id, region) = (spec.run_id.to_string(), region.to_string());
                tracing::warn!(run_id = %run_id, region = %region, why = %why, "a region that reseeds on continue could not be seeded; it is left empty");
            }
        }
    }
    out
}

/// Reopen the finished run `run`, read back from `run_dir`, for the segment
/// `message` starts: a fork's first, when it was `forked_from` another run.
///
/// The segment that ended keeps its answer under `segments/<n>/` beside the
/// run file, since the next one's answer is written where the run's answer
/// always is. Nothing else is written: the run file is untouched until the
/// reopened run's first step.
pub fn prepare(
    run_dir: &Path,
    mut run: Resumable,
    message: ContinueMessage,
    forked_from: Option<RunId>,
) -> Result<Prepared, SpawnIssues> {
    check(&run.spec, &run.state)?;
    check_message(&run.spec.graph, &run.state, &message)?;
    let stage = run
        .spec
        .graph
        .continue_stage()
        .map(|s| s.name.clone())
        .expect("a checked graph has the stage it continues at");
    let previous = match forked_from {
        Some(_) => None,
        None => Some(SegmentEnd::of(
            &run.state,
            keep_answer(run_dir, run.segment, &run.state)?,
        )),
    };
    let segment = match forked_from {
        Some(_) => 1,
        None => run.segment + 1,
    };
    reopen(&mut run.state);
    run.answer = None;
    run.segment = segment;
    Ok(Prepared {
        run,
        reseeded: BTreeMap::new(),
        continued: ContinuedState {
            segment,
            stage,
            forked_from,
            previous,
            regions: Vec::new(),
        },
        message,
    })
}

/// Refuse a message that says nothing, or one sent to a region the run does
/// not have.
fn check_message(
    graph: &RunGraph,
    state: &RunState,
    message: &ContinueMessage,
) -> Result<(), SpawnIssues> {
    if message.text.trim().is_empty() && message.parts.is_empty() {
        return Err(refused(
            IssueCode::Missing,
            SpecPath::root().field("text"),
            "a continuation needs a message: give it some text or a file",
        ));
    }
    let Some(region) = &message.region else {
        return Ok(());
    };
    let known = known_regions(graph, state);
    if known.iter().any(|k| k == region) {
        return Ok(());
    }
    let mut issue = SpawnIssue::new(
        SpecPath::root().field("region"),
        IssueCode::Unknown,
        format!("the run has no region named \"{region}\""),
    );
    issue.known = known;
    Err(issue.into())
}

/// Every region a run of `graph`, standing in `state`, can take a message
/// in: those its layouts declare, those its window holds, and those every
/// stage sees.
fn known_regions(graph: &RunGraph, state: &RunState) -> Vec<String> {
    let mut names: Vec<String> = std::iter::once(&graph.layout)
        .chain(graph.stages.iter().filter_map(|s| s.layout.as_ref()))
        .flat_map(|layout| layout.regions.iter().map(|r| r.name.to_string()))
        .chain(state.context.regions.iter().map(|r| r.name.to_string()))
        .chain(
            crate::spec::graph::ALWAYS_VISIBLE_REGIONS
                .iter()
                .map(|s| s.to_string()),
        )
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Keep the answer of segment `segment`, which `state` ended with, under
/// `segments/<segment>/` in `run_dir`. `None` when it handed back none.
fn keep_answer(
    run_dir: &Path,
    segment: u32,
    state: &RunState,
) -> Result<Option<FileRef>, SpawnIssues> {
    let Some(file) = &state.files.final_output else {
        return Ok(None);
    };
    let unreadable = |why: String| {
        refused(
            IssueCode::Unavailable,
            SpecPath::root(),
            format!("the run's answer cannot be kept for its next segment: {why}"),
        )
    };
    let bytes = file.read(run_dir).map_err(|e| unreadable(e.to_string()))?;
    let path = kept_answer_path(segment);
    let target = crate::state::under(run_dir, &path);
    let written = target
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| leviath_sys::write_private(&target, &bytes));
    written.map_err(|e| unreadable(e.to_string()))?;
    Ok(Some(FileRef::whole(path, &bytes)))
}

/// Reopen `state` for a new segment: going again, with nothing in flight and
/// nothing owed from the segment that ended. The context, the spend, the
/// ledger and the title carry over; the visits per stage start again, so a
/// stage's revisit limit counts this segment's own visits.
fn reopen(state: &mut RunState) {
    state.status = RunStatus::Active;
    state.phase = PipelinePhase::ReadyToInfer;
    state.pending = None;
    state.fan_out = None;
    state.inbox.clear();
    state.interactions.clear();
    state.wait_reason = None;
    state.held = None;
    state.final_output = None;
    state.files.final_output = None;
    state.visits.clear();
    state.point = Default::default();
    state.remote_jobs.clear();
    state.last_progress_at = None;
    let flags = &mut state.flags;
    flags.empty_output = false;
    flags.produced_output = false;
    flags.output_forced = 0;
    flags.max_iterations_hit = 0;
    flags.gates_forced = 0;
    flags.required_regions_abandoned.clear();
}

/// Place a reopened run in the world, with the live handles binding its spec
/// produced, for `apply` to continue.
pub fn place(
    world: &mut World,
    prepared: Prepared,
    bindings: crate::spec::env::Bindings,
) -> Entity {
    let Prepared {
        run,
        reseeded,
        continued,
        message,
    } = prepared;
    let spec = run.spec.clone();
    let entity = crate::restore::resume(world, run, bindings);
    world.entity_mut(entity).insert(Reopened {
        spec,
        continued,
        reseeded,
        message,
    });
    entity
}

/// A continuation applied in the world.
#[derive(Debug)]
pub(crate) struct Applied {
    /// The segment it started.
    pub(crate) segment: u32,
    /// The stage it started in.
    pub(crate) stage: String,
    /// The run it was forked from.
    pub(crate) forked_from: Option<String>,
    /// Where the step that reopened the run landed, once it has.
    pub(crate) landed: oneshot::Receiver<Appended>,
}

/// Continue the run on `entity`, placed by [`place`]: carry each region over
/// by its policy, enter the stage the graph continues at, leave the message
/// waiting for the summaries, and record the step that reopened the run.
/// `None` for an entity no continuation was placed on.
pub(crate) fn apply(world: &mut World, entity: Entity) -> Option<Applied> {
    let Reopened {
        spec,
        mut continued,
        reseeded,
        message,
    } = world.entity_mut(entity).take::<Reopened>()?;
    let run_id = spec.run_id.to_string();
    let compact = {
        let mut window = world
            .get_mut::<ContextWindow>(entity)
            .expect("a placed run has a window");
        continued.regions = carry(&spec.graph, &mut window, &reseeded);
        compacting_now(&continued.regions, &window)
    };
    let idx = spec
        .graph
        .stages
        .iter()
        .position(|s| s.name == continued.stage)
        .unwrap_or_default();
    let agent = crate::world::AgentId::in_world(world, entity);
    crate::pipeline::force_transition(world, agent, idx);
    world
        .get_mut::<MessageInbox>(entity)
        .expect("a placed run has an inbox")
        .push(AgentMessage {
            agent_id: run_id.clone(),
            from: FROM_CONTINUATION.to_string(),
            content: message.text,
            target_region: message.region,
            parts: message.parts,
        });
    if !compact.is_empty() {
        world.entity_mut(entity).insert(PendingEdgeCompact(compact));
    }
    let applied = (
        continued.segment,
        continued.stage.to_string(),
        continued.forked_from.as_ref().map(ToString::to_string),
    );
    let record = crate::runfile::record::RunRecord::Continued(Box::new(continued));
    let landed = match world.get_resource::<JournalSender>() {
        Some(journal) => journal.record_acked(&run_id, record),
        None => answered(Appended::NoJournal),
    };
    Some(Applied {
        segment: applied.0,
        stage: applied.1,
        forked_from: applied.2,
        landed,
    })
}

/// A receiver already answered with `landed`.
fn answered(landed: Appended) -> oneshot::Receiver<Appended> {
    let (tx, rx) = oneshot::channel();
    let _ = tx.send(landed);
    rx
}

/// Carry each region of `window` over by its policy: empty the ones that
/// clear or reseed (one transaction), then fill the ones that reseed from
/// `reseeded`. Returns every region's policy, in window order.
fn carry(
    graph: &RunGraph,
    window: &mut ContextWindow,
    reseeded: &BTreeMap<String, SeededContent>,
) -> Vec<(String, OnContinue)> {
    let policies: Vec<(String, OnContinue)> = window
        .regions
        .iter()
        .map(|r| (r.name.clone(), policy_of(graph, r)))
        .collect();
    let emptied: Vec<&str> = policies
        .iter()
        .filter(|(_, p)| matches!(p, OnContinue::Clear | OnContinue::Reseed))
        .map(|(name, _)| name.as_str())
        .collect();
    let txn = window.begin_changes(emptied.iter().copied());
    for name in &emptied {
        window
            .get_region_mut(name)
            .into_iter()
            .for_each(|region| region.clear());
    }
    window.current_tokens = window.calculate_tokens();
    window.commit_change(
        leviath_core::ContextCause::Transform,
        txn,
        crate::components::Pushed::Nothing,
    );
    for (name, policy) in &policies {
        if let (OnContinue::Reseed, Some(seeded)) = (policy, reseeded.get(name)) {
            crate::insert::seed_region(window, name, seeded);
        }
    }
    window.current_tokens = window.calculate_tokens();
    policies
}

/// What the region `region` does when a run of `graph` is continued: the
/// policy its layouts set, or the one its kind follows.
fn policy_of(graph: &RunGraph, region: &leviath_core::Region) -> OnContinue {
    graph
        .continue_policy(&region.name)
        .unwrap_or_else(|| OnContinue::for_kind(&region.kind))
}

/// The regions among `policies` that compact and have something in `window`
/// to summarize.
fn compacting_now(policies: &[(String, OnContinue)], window: &ContextWindow) -> Vec<String> {
    policies
        .iter()
        .filter(|(_, policy)| *policy == OnContinue::Compact)
        .filter(|(name, _)| {
            window
                .get_region(name)
                .is_some_and(|r| r.summarizable && !r.content.is_empty())
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// On a run placed with its continuation's message still waiting, ask again
/// for the summaries the continuation wanted. A restart came between the run
/// reopening and the message landing, and the message lands on the tick its
/// summaries do, so they had not landed either.
pub(crate) fn resume_waiting(entity: &mut EntityWorldMut<'_>, spec: &RunSpec, state: &RunState) {
    if !state.inbox.iter().any(|m| m.from == FROM_CONTINUATION) {
        return;
    }
    let compact = entity
        .get::<ContextWindow>()
        .map(|window| {
            let policies: Vec<(String, OnContinue)> = window
                .regions
                .iter()
                .map(|r| (r.name.clone(), policy_of(&spec.graph, r)))
                .collect();
            compacting_now(&policies, window)
        })
        .unwrap_or_default();
    if !compact.is_empty() {
        entity.insert(PendingEdgeCompact(compact));
    }
}

/// Hear when everything the persistence lane holds for `run_id` has been
/// written: answered once the lane reaches this, behind every earlier write.
/// Answered at once in a world that keeps no files.
pub(crate) fn lane_barrier(world: &World, run_id: &str) -> oneshot::Receiver<Appended> {
    let Some(lane) = world.get_resource::<crate::pipeline::PersistenceStage>() else {
        return answered(Appended::NoJournal);
    };
    let (ack, landed) = oneshot::channel();
    let mut step = crate::runfile::lane::RunFileStep::events(
        run_id,
        chrono::Utc::now().timestamp(),
        Vec::new(),
    );
    step.acks.push(ack);
    let _ = lane
        .0
        .send(crate::persistence_bridge::PersistMsg::Step(Box::new(step)));
    landed
}

/// Write the run forked from `source` as a new run `run_id` under
/// `runs_dir`: the finished run's spec and code, and its last state as the
/// new run's first, with nothing it spent, wrote or was granted carried over,
/// and its stored parts copied beside the new file. The new run is what a
/// continuation then reopens; `source`'s own files are only read.
pub fn fork(
    source: &Resumable,
    source_dir: &Path,
    run_id: RunId,
    runs_dir: &Path,
) -> Result<Resumable, SpawnIssues> {
    check(&source.spec, &source.state)?;
    let dir = runs_dir.join(run_id.as_str());
    let unwritable = |why: String| {
        refused(
            IssueCode::Unavailable,
            SpecPath::root(),
            format!("the fork cannot be written: {why}"),
        )
    };
    let mut spec = (*source.spec).clone();
    spec.delivery.metadata.insert(
        FORKED_FROM_LABEL.to_string(),
        source.spec.run_id.to_string(),
    );
    spec.run_id = run_id;
    spec.created_at = chrono::Utc::now().timestamp();
    spec.delivery.callback = None;
    spec.delivery.idempotency = None;
    spec.listed = None;
    let mut state = source.state.clone();
    state.seq = 0;
    state.totals = Default::default();
    state.clock = Default::default();
    state.ledger = spec
        .graph
        .stages
        .iter()
        .map(|s| crate::insert::place::pending_stage(s.name.clone()))
        .collect();
    state.children.clear();
    state.files = Default::default();
    state.final_output = None;
    state.grants = Default::default();
    state.written = 0;
    state.flags = Default::default();
    std::fs::create_dir_all(&dir).map_err(|e| unwritable(e.to_string()))?;
    for blob in &state.blobs {
        let from = crate::runfile::blob_path(source_dir, &blob.digest);
        let to = crate::runfile::blob_path(&dir, &blob.digest);
        to.parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::copy(&from, &to).map(drop))
            .map_err(|e| unwritable(format!("stored part {}: {e}", blob.digest)))?;
    }
    let path = dir.join(leviath_core::files::RUN_FILE);
    crate::runfile::RunFileWriter::create(&path, &spec, &source.code, &state, Default::default())
        .map_err(|e| unwritable(e.to_string()))?;
    Ok(Resumable {
        spec: std::sync::Arc::new(spec),
        state,
        code: source.code.clone(),
        answer: None,
        asked: 0,
        audit: Vec::new(),
        segment: 0,
    })
}

/// The label a fork carries in its metadata, naming the run it was forked
/// from.
pub const FORKED_FROM_LABEL: &str = "leviath.forked_from";

#[cfg(test)]
#[path = "continuation_tests.rs"]
mod tests;
