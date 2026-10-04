//! The fan-out engine: many sub-agents at once, and their results together.
//!
//! # One engine, two entry points
//!
//! Both start at `begin_fan_out`, which parks the parent in [`FanOutWaiting`].
//! `fan_out_collect` then starts one worker per item - bounded by
//! `max_workers` - through the daemon-installed [`FanOutSpawner`], tracks them
//! as the parent's `SubAgentChildren`, and once every worker is terminal applies
//! the failure policy and builds the consolidated report.
//!
//! What differs is only how that report is delivered, which is exactly what
//! [`FanOutOrigin`] records:
//!
//! - **The [`FAN_OUT_TOOL`] tool**, callable from any stage that grants it. The
//!   dispatcher reads the call (`parse_fan_out_call`), hands it over as
//!   `PendingFanOut`, and `start_pending_fan_outs` begins it. The report
//!   comes back as that call's tool result, routed by the stage's
//!   `tool_routing` like any other, and the agent carries on where it was.
//! - **A fan-out stage** (see [`crate::spec::graph::StageMode::FanOut`]),
//!   which is sugar for granting the same tool: its report goes to the
//!   stage's `results_region` and the stage transitions to its `merge_stage`.
//!
//! Because both park the same way, both survive a daemon restart the same
//! way (see [`FanOutState`]).
//!
//! # What lives elsewhere
//!
//! The runtime only **starts and tracks** workers. It hands the installed
//! [`FanOutSpawner`] a [`SpawnRequest`] per work item, carrying the item's
//! typed inputs; resolving and binding that request is the host's job.
//!
//! A single sub-agent is `spawn_agent`, not a fan-out of one.
//!
//! [`FAN_OUT_TOOL`]: leviath_core::stage_tools::FAN_OUT_TOOL
mod adopt;
mod io;
mod items;
mod report;
mod starts;
mod worker_sources;
pub use adopt::{UnrecordedWorker, settle_unrecorded_worker};
pub(crate) use io::{FanOutIo, ReadingWorkerInputs};
pub(crate) use items::{FanOutRequest, config_for, is_fan_out_tool, parse_fan_out_call};
pub use items::{WORK_ITEM_LABEL, WorkItem};
use report::*;
pub(crate) use starts::WorkerStarts;
pub use starts::{PlaceWorker, WorkerPrep};
use worker_sources::merge_worker_sources;

use std::collections::VecDeque;
use std::sync::Arc;

use crate::insert::RunSpecC;
use crate::spec::env::Caller;
use crate::spec::graph::{FanOutDef, StageMode, WorkerFailure, WorkerSource};
use crate::spec::inputs::InputDecl;
use crate::spec::names::BlueprintRef;
use crate::spec::request::{SpawnRequest, SpawnSource};
use bevy_ecs::prelude::*;

use crate::blob_store::{BlobStoreHandle, MimeLimits, MimeRegistryHandle, RunMimeRegistry};
use crate::context_setup::PartSink;

use crate::components::{
    AgentState, AgentStatus, ContextWindow, InferenceResult, ParentRef, SubAgentChildren,
};
use crate::pipeline::{ResolveTransition, StageCursor};

/// Depth cap for fan-out workers when the parent's graph doesn't set one.
const DEFAULT_FANOUT_DEPTH: usize = 3;

/// Starts one worker for a fan-out work item. The implementor resolves and
/// binds `request` for `caller` (a [`Caller::Worker`] naming the parent, its
/// policy and depth, and the stage a same-graph worker enters) off the world's
/// tick, and says how to place what it made. Parent/child linking is done by
/// `fan_out_collect`, not the spawner.
pub trait FanOutSpawner: Send + Sync {
    /// Prepare one worker: everything that reads or writes outside the world
    /// (resolving, recording, binding) happens in the returned future, which
    /// the world runs off its tick. It resolves to how to place the worker, or
    /// `Err` with a human-readable reason (recorded as that item's failure).
    fn prepare_worker(&self, request: SpawnRequest, caller: Caller) -> WorkerPrep;

    /// The installed blueprint a fan-out's worker query picks, or `Err` with
    /// why none does.
    fn find_worker(&self, query: &str) -> Result<BlueprintRef, String>;

    /// The inputs the blueprint `source` names declares, so a fan-out's items
    /// are checked against them before any worker starts. `None` when this
    /// spawner cannot read it; each worker's own spawn checks them then.
    fn worker_inputs(&self, _source: &SpawnSource) -> Option<Vec<InputDecl>> {
        None
    }
}

/// The installed [`FanOutSpawner`], as a world resource. Absent in a pure-runtime
/// world (then every fan-out item fails with "no fan-out spawner installed").
#[derive(Resource, Clone)]
pub struct FanOutSpawnerRes(pub Arc<dyn FanOutSpawner>);

/// A currently-running fan-out worker: its work-item id, its live entity, and
/// its run-id (kept so the waiting state can be persisted/restored without a
/// cross-entity lookup - see [`FanOutState`]).
struct ActiveWorker {
    item_id: String,
    entity: Entity,
    run_id: String,
}

/// How a fan-out was started, and so how its results come back.
///
/// One engine, two entry points. The workers, the concurrency cap, the failure
/// policy and the merged report are identical either way; only the last step
/// differs, and this is the whole of that difference.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub enum FanOutOrigin {
    /// A fan-out stage. The report goes to the stage's `results_region` and
    /// the stage transitions to its `merge_stage`.
    ///
    /// The default, so a state that does not say reads as a stage.
    #[default]
    Stage,
    /// A `fan_out` tool call from an ordinary stage. The report comes back as
    /// that call's result - routed by the stage's `tool_routing` like any other,
    /// so the graph decides where it lands or whether it lands at all - and
    /// the agent carries on where it left off.
    Tool {
        /// The tool call this fan-out is the result of.
        call_id: String,
    },
}

/// A parent parked while its fan-out workers run. Holds the not-yet-started
/// `pending` items, the currently-`active` workers, and the accumulated results.
#[derive(Component)]
pub struct FanOutWaiting {
    config: FanOutDef,
    max_workers: Option<usize>,
    pending: VecDeque<WorkItem>,
    /// Items whose workers are being prepared off the tick (see
    /// [`starts`]). They count as running against the concurrency cap.
    starting: Vec<WorkItem>,
    /// Finished workers whose files are being copied up off the tick (see
    /// [`io`]). The fan-out does not finish until they land.
    handing_up: usize,
    active: Vec<ActiveWorker>,
    summaries: Vec<(String, String)>,
    failures: Vec<(String, String)>,
    /// The files finished workers handed back, re-stored under this parent's
    /// run so the merge stage's model can take them as parts.
    parts: Vec<leviath_core::mime::Part>,
    /// Set when the user pauses this parent. Its own status has to stay
    /// `Waiting` - the merge poll reads it - so the pause lives here instead,
    /// and holds back the one thing a parked parent still does on its own:
    /// starting the next queued worker. Without it, pausing a fan-out would
    /// pause the running children and immediately launch their replacements.
    paused: bool,
    /// Which entry point started this, and so how its report is delivered.
    origin: FanOutOrigin,
}

/// The serializable form of [`FanOutWaiting`], so a parent interrupted
/// mid-split resumes its merge after a restart. `active`
/// carries worker **run-ids** (not entities); recovery maps them back to the
/// reloaded worker entities.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FanOutState {
    /// The fan-out configuration.
    pub config: FanOutDef,
    /// The concurrency cap; `None` runs every item at once.
    pub max_workers: Option<usize>,
    /// Work items not yet started.
    pub pending: Vec<WorkItem>,
    /// In-flight workers as `(item_id, run_id)`.
    pub active: Vec<(String, String)>,
    /// Completed worker results as `(item_id, summary)`.
    pub summaries: Vec<(String, String)>,
    /// Failed worker results as `(item_id, message)`.
    pub failures: Vec<(String, String)>,
    /// The parts finished workers handed back, already in the parent's store.
    /// `default` so a state without the key loads, with none.
    #[serde(default)]
    pub parts: Vec<leviath_core::mime::Part>,
    /// Whether the fan-out was paused. `default` so a state without the key
    /// loads, as an un-paused one.
    #[serde(default)]
    pub paused: bool,
    /// Which entry point started this. `default` (a stage) for the same reason.
    #[serde(default)]
    pub origin: FanOutOrigin,
}

impl FanOutWaiting {
    /// Workers this parent is still parked on: in-flight plus not-yet-started.
    ///
    /// Surfaced by `lev ps` so "waiting" on a fan-out parent reads as progress
    /// against a known denominator rather than an unexplained stall.
    pub(crate) fn outstanding(&self) -> usize {
        self.running() + self.pending.len()
    }

    /// Workers running or being started.
    fn running(&self) -> usize {
        self.active.len() + self.starting.len()
    }

    /// How far the fan-out has got: its queued, running, done and failed
    /// workers, counted. Any worker starting or finishing changes it.
    pub(crate) fn progress(&self) -> [usize; 4] {
        [
            self.pending.len(),
            self.running(),
            self.summaries.len(),
            self.failures.len(),
        ]
    }

    /// Whether this parent's fan-out is paused (see the field).
    #[cfg(test)]
    pub(crate) fn is_paused(&self) -> bool {
        self.paused
    }

    /// Latch or release the fan-out. Returns whether this changed anything, so
    /// a caller can tell a real pause from a repeat.
    pub(crate) fn set_paused(&mut self, paused: bool) -> bool {
        let changed = self.paused != paused;
        self.paused = paused;
        changed
    }

    /// Project to the serializable [`FanOutState`] (workers by run-id).
    pub(crate) fn to_state(&self) -> FanOutState {
        FanOutState {
            config: self.config.clone(),
            origin: self.origin.clone(),
            max_workers: self.max_workers,
            // A worker still being prepared has no run of its own to come
            // back to yet, so it is saved as still to start.
            pending: self
                .starting
                .iter()
                .chain(self.pending.iter())
                .cloned()
                .collect(),
            active: self
                .active
                .iter()
                .map(|w| (w.item_id.clone(), w.run_id.clone()))
                .collect(),
            summaries: self.summaries.clone(),
            failures: self.failures.clone(),
            parts: self.parts.clone(),
            paused: self.paused,
        }
    }
}

/// Rebuild a parent's [`FanOutWaiting`] from a persisted [`FanOutState`] and
/// insert it, mapping each active worker's run-id back to its reloaded entity
/// via `resolve`. Workers whose entity didn't reload are treated as failures so
/// the merge still completes rather than waiting forever. Used by restart
/// recovery to resume an interrupted fan-out.
pub fn restore_fan_out_waiting(
    world: &mut World,
    parent: Entity,
    state: FanOutState,
    resolve: &dyn Fn(&str) -> Option<Entity>,
) {
    let mut active = Vec::new();
    let mut failures = state.failures;
    for (item_id, run_id) in state.active {
        match resolve(&run_id) {
            Some(entity) => active.push(ActiveWorker {
                item_id,
                entity,
                run_id,
            }),
            None => failures.push((item_id, "worker did not reload after restart".to_string())),
        }
    }
    world.entity_mut(parent).insert(FanOutWaiting {
        origin: state.origin.clone(),
        config: state.config,
        max_workers: state.max_workers,
        pending: state.pending.into_iter().collect(),
        starting: Vec::new(),
        handing_up: 0,
        active,
        summaries: state.summaries,
        failures,
        parts: state.parts,
        paused: state.paused,
    });
}

/// Set on an agent that has started a fan-out in its current stage.
///
/// Cleared on stage entry, unlike [`PreviousWorkItems`], because the two answer
/// different questions: "has this entry fanned out yet" gates the nudge, and
/// "what did the last round cover" is what the next round is told.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FannedOut;

/// The ids a fan-out stage's last split handed out, so a later split of the same
/// stage can be told what has already been researched.
///
/// Set on every successful split and read only on a re-entry. Absent means this
/// stage has not split before.
#[derive(Component, Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PreviousWorkItems(pub Vec<String>);

/// How many previous work-item ids the re-entry framing lists before it stops.
///
/// A fan-out can legitimately be thirty items wide, and thirty slugs at the top
/// of a prompt is a wall the instruction after it has to compete with.
const FRAMED_PREVIOUS_ITEMS: usize = 12;

/// What [`frame_split_round`] selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about lifetimes:
/// the borrow is bound when the query is fetched.
type FrameSplitRoundQuery = (
    Entity,
    &'static RunSpecC,
    &'static StageCursor,
    &'static crate::pipeline::VisitCounts,
    &'static mut ContextWindow,
    Option<&'static PreviousWorkItems>,
);

/// Tell a fan-out stage it has been here before.
///
/// The failure this exists for: a `deep-researcher` run finished its fan-out,
/// ran `analyze`, routed back through `gather`, and re-entered the same fan-out
/// stage. The split prompt was byte for byte the one it had already answered,
/// while `conversation` still carried the first split, the workers'
/// consolidated report and the analysis built on it. The model read all that and
/// answered "I have completed the research", which is true and is not a list of
/// work items. Two corrections later the run was dead.
///
/// So the second split is asked a different question from the first, and told
/// that an empty list is a real answer to it. The stage's own `split_prompt` is
/// unchanged, and a first entry is not touched at all - no framing, no extra
/// tokens, no behaviour change for the run that splits once.
pub(crate) fn frame_split_round(
    mut agents: Query<FrameSplitRoundQuery, With<crate::pipeline::StageJustEntered>>,
) {
    crate::tick_scope::clear();
    for (entity, spec, cursor, visits, mut window, previous) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        let stage = &spec.0.graph.stages[cursor.index];
        if !matches!(stage.mode, StageMode::FanOut(_)) {
            continue;
        }
        // `enter_stage` bumps the count before this runs, so a first entry reads
        // as 1 and there is nothing to say.
        let round = visits.0.get(stage.name.as_str()).copied().unwrap_or(1);
        if round < 2 {
            continue;
        }
        crate::pipeline::inject_system_nudge(
            &mut window,
            &split_round_framing(round, previous.map_or(&[], |p| p.0.as_slice())),
        );
    }
}

/// What a re-entered fan-out stage is told before it splits again.
fn split_round_framing(round: usize, previous: &[String]) -> String {
    let tool = leviath_core::stage_tools::FAN_OUT_TOOL;
    let already = match previous.is_empty() {
        // A previous round whose ids were lost - a daemon restart between the
        // two entries drops the component - still gets the framing, because the
        // part that matters is "you have been here before", not the list.
        true => "Work has already been handed out from this stage once".to_string(),
        false => format!(
            "These work items have already been researched, and their findings are \
             in this run's context:\n{}{}",
            previous
                .iter()
                .take(FRAMED_PREVIOUS_ITEMS)
                .map(|id| format!("  - {id}\n"))
                .collect::<String>(),
            match previous.len() > FRAMED_PREVIOUS_ITEMS {
                true => format!("  ...and {} more\n", previous.len() - FRAMED_PREVIOUS_ITEMS),
                false => String::new(),
            }
        ),
    };
    format!(
        "This is split round {round} of this stage. {already}.\n\nName ONLY \
         sub-questions that are still unanswered - do not hand out work that has \
         already been done, and do not restate the previous round. If nothing is \
         left to hand out, call `{tool}` with an empty `items` array: the run then \
         moves on to the next stage, which is the right outcome when the work is \
         finished. Answering that in prose is not."
    )
}

/// How many workers one pass of [`fan_out_collect`] will start before
/// handing the tick back. A bound on how long one fan-out can hold the driver
/// thread; the queue drains over the following passes of the same wake.
pub(crate) const MAX_WORKER_STARTS_PER_PASS: usize = 4;

/// How many agents one run may create, sub-agents included, or `0` for no limit.
///
/// Read at every fan-out spawn. A run at its ceiling stops widening and finishes
/// on what it has, rather than failing: the work already done is worth keeping,
/// and a run that stopped early is a cheaper answer, not a broken one.
///
/// The operator's number rather than the blueprint's. Cost per agent is stable
/// (measured $5.37 to $9.05 across four runs) while the count is not (10 to 42),
/// so this is the knob that decides what a run costs - and whose account it
/// costs it to is not something a blueprint author can know.
#[derive(bevy_ecs::prelude::Resource, Clone, Copy, Debug, Default)]
pub struct FanOutBudget(pub usize);

/// Every agent in this run's tree, counted from its root.
///
/// Walked from the root rather than the spawning parent: a depth-2 worker asking
/// "how many of us are there" must not answer with the size of its own branch.
fn run_tree_size(world: &World, entity: Entity) -> usize {
    let mut root = entity;
    while let Some(parent) = world.get::<ParentRef>(root) {
        root = parent.parent_entity;
    }
    fn count(world: &World, at: Entity) -> usize {
        1 + world
            .get::<SubAgentChildren>(at)
            .map(|kids| {
                kids.children
                    .iter()
                    .map(|c| count(world, *c))
                    .sum::<usize>()
            })
            .unwrap_or(0)
    }
    count(world, root)
}

/// The item ceiling this run's graph declares on any fan-out stage it has.
///
/// `None` when it declares none, which leaves a tool-driven split unbounded, as
/// it has always been. Read from the graph rather than the current stage on
/// purpose: the tool is called from ordinary stages, which is the whole reason
/// the ceiling was being missed.
fn graph_fan_out_max_items(world: &World, entity: Entity) -> Option<u32> {
    world
        .get::<RunSpecC>(entity)?
        .0
        .graph
        .stages
        .iter()
        .find_map(|stage| match &stage.mode {
            StageMode::FanOut(def) => def.max_items,
            _ => None,
        })
}

/// A `fan_out` call the dispatcher accepted, waiting for a tick with world
/// access to start it.
///
/// The hand-off exists because starting a fan-out is world work - it resolves
/// the worker blueprint through the injected spawner - and `dispatch_tools` is
/// an ordinary system. The same shape the interaction and gate-prompt lanes use.
#[derive(Component, Debug, Clone)]
pub(crate) struct PendingFanOut {
    /// The tool call whose result this fan-out will be.
    pub call_id: String,
    /// What the model asked for.
    pub request: FanOutRequest,
}

/// Start every fan-out the dispatcher accepted this tick (exclusive).
///
/// Ordered before [`fan_out_collect`], so a fan-out started here has its workers
/// launched on the same tick rather than a tick later.
pub(crate) fn start_pending_fan_outs(world: &mut World) {
    crate::tick_scope::clear();
    io::land_inputs(world);
    // A call waits while its worker's inputs are read, and while its agent
    // is paused; one whose agent stopped is never started.
    let pending: Vec<(Entity, PendingFanOut)> = {
        let mut q = world
            .query_filtered::<(Entity, &PendingFanOut, &AgentState), Without<ReadingWorkerInputs>>(
            );
        q.iter(world)
            .filter(|(_, _, s)| s.status == AgentStatus::Active)
            .map(|(e, p, _)| (e, p.clone()))
            .collect()
    };
    for (entity, PendingFanOut { call_id, request }) in pending {
        crate::tick_scope::enter(entity);
        let mut agent = world.entity_mut(entity);
        agent.remove::<PendingFanOut>();
        let read = agent.take::<io::WorkerInputs>();
        // A fan-out stage's own keys when there are any, so a stage that set
        // `max_items` or `on_worker_failure` still gets them; nothing when an
        // ordinary stage called the tool.
        let spec = world.get::<RunSpecC>(entity).map(|s| s.0.clone());
        let stage_config = world
            .get::<StageCursor>(entity)
            .zip(spec.as_ref())
            .and_then(
                |(cursor, spec)| match &spec.graph.stages[cursor.index].mode {
                    StageMode::FanOut(def) => Some(def.clone()),
                    _ => None,
                },
            );
        // Which door this came through, and so how its report is delivered. A
        // `mode = "fan_out"` stage answers with the same tool call as anybody
        // else, so the call cannot tell us - only the stage can.
        //
        // Getting this wrong made `results_region` and `merge_stage` dead
        // config: a live `deep-researcher` fan-out delivered three workers'
        // findings into `conversation` as a tool result and resumed the split
        // stage, instead of writing `sub_findings` and moving to `analyze`. The
        // unit tests passed throughout, because they build the origin directly
        // and never went through this decision.
        let origin = match stage_config.is_some() {
            true => FanOutOrigin::Stage,
            false => FanOutOrigin::Tool {
                call_id: call_id.clone(),
            },
        };
        let workdir = spec.as_ref().map(|s| s.placement.workdir.clone());
        let mut config = match config_for(&request, stage_config.as_ref(), workdir.as_deref()) {
            Ok(config) => config,
            Err(why) => {
                answer_call(world, entity, &call_id, format!("[error] {why}"));
                continue;
            }
        };
        // A worker takes the inputs of the graph it runs, so its items are
        // checked here against them, before any worker starts, each at its
        // item's path: this run's own graph for a worker stage, the named
        // blueprint's when the spawner can read it. Reading a blueprint is
        // I/O, so it is done off the tick and the call waits for it.
        let known = match (io::WorkerBlueprint::of(&config.worker), read) {
            (None, _) => Ok(spec.as_ref().map(|s| s.graph.inputs.clone())),
            (Some(_), Some(io::WorkerInputs(decls))) => Ok(decls),
            (Some(blueprint), None) => Err(blueprint),
        };
        let decls = match known.map_err(|blueprint| io::read_inputs(world, entity, blueprint)) {
            Ok(decls) | Err(Some(decls)) => decls,
            Err(None) => {
                world
                    .entity_mut(entity)
                    .insert((PendingFanOut { call_id, request }, ReadingWorkerInputs));
                continue;
            }
        };
        if let Some(decls) = decls
            && let Err(issues) = items::check_items(&decls, &request.items)
        {
            answer_call(
                world,
                entity,
                &call_id,
                format!("[error] fan_out items do not fit the worker's inputs:\n{issues}"),
            );
            continue;
        }
        // A call through the tool comes from an ordinary stage, so it carries no
        // `max_items` and creates as many workers as the model named. Where the
        // graph declares a fan-out stage, that stage's ceiling is the
        // author's answer to "how wide should a split of this work be", and a
        // split of this work is what this is. Measured: a blueprint saying
        // `max_items = 3` produced six-way splits through this door, and one run
        // reached 34 sub-agents where an earlier one reached 7.
        //
        // Only the ceiling is inherited. `worker_agent`, `merge_stage` and
        // `results_region` describe how a *stage* delivers its report, and
        // taking those would change where this call's result goes.
        if config.max_items.is_none() {
            config.max_items = graph_fan_out_max_items(world, entity);
        }
        begin_fan_out(world, entity, config, request.items, origin);
    }
}

/// Start a fan-out: park `parent` on its workers.
///
/// The single way in. Both entry points - the `fan_out` tool from any stage, and
/// a `fan_out` stage's own call - land here with a config and a list, and
/// everything after this point is [`fan_out_collect`] regardless of which it was.
///
/// An empty list is allowed and is not a special case: the parent parks with
/// nothing pending, the collector finds nothing running, and it finishes on the
/// next tick with an empty report. That is what "there is nothing to hand out"
/// has to do, and making it a separate path is how it would drift.
pub(crate) fn begin_fan_out(
    world: &mut World,
    parent: Entity,
    config: FanOutDef,
    items: Vec<WorkItem>,
    origin: FanOutOrigin,
) {
    let max_workers = items::worker_cap(&config);
    // A caller decides its own item count, so without a cap a model that returns
    // five hundred items spawns five hundred runs. The cap also fixes each
    // worker's share of the results region: past some number of ways to divide
    // it, every section is too small to say anything.
    let items = match config.max_items.map(|cap| cap as usize) {
        Some(cap) if items.len() > cap => {
            tracing::warn!(
                produced = items.len(),
                cap,
                "fan_out produced more items than max_items; keeping the first"
            );
            items.into_iter().take(cap).collect::<Vec<_>>()
        }
        _ => items,
    };
    // Kept for the next entry into this stage, which is told what was already
    // handed out rather than being asked the same question over a context that
    // answers it.
    world
        .entity_mut(parent)
        .insert(PreviousWorkItems(
            items.iter().map(|i| i.id.clone()).collect(),
        ))
        .insert(FannedOut)
        .insert(FanOutWaiting {
            config,
            max_workers,
            pending: items.into_iter().collect(),
            starting: Vec::new(),
            handing_up: 0,
            active: Vec::new(),
            summaries: Vec::new(),
            failures: Vec::new(),
            parts: Vec::new(),
            paused: false,
            origin,
        });
    set_status(world, parent, AgentStatus::Waiting);
}

/// Fan-out collect system (exclusive): drive each [`FanOutWaiting`] parent - reap
/// finished workers, start pending ones up to `max_workers`, and once none remain
/// running apply the failure policy, inject the consolidated report, and
/// transition to the merge stage (or resolve the stage's own transition).
pub(crate) fn fan_out_collect(world: &mut World) {
    crate::tick_scope::clear();
    let parents: Vec<Entity> = {
        let mut q = world.query_filtered::<Entity, With<FanOutWaiting>>();
        q.iter(world).collect()
    };
    // The worker starts that finished off the tick since the last pass. One
    // whose parent is not waiting on it any more is placed and cancelled.
    let mut landed = starts::drain(world);
    let (mine, orphans): (Vec<_>, Vec<_>) = landed
        .drain(..)
        .partition(|l| parents.contains(&l.parent()));
    for orphan in orphans {
        starts::abandon(world, orphan);
    }
    let mut landed = mine;
    // The copies of finished workers' files that landed since the last pass.
    let mut handed = io::drain_hand_ups(world);

    for parent in parents {
        crate::tick_scope::enter(parent);
        let (here, rest): (Vec<_>, Vec<_>) = landed.drain(..).partition(|l| l.parent() == parent);
        landed = rest;
        // A cancelled/errored parent abandons the fan-out; its workers are reaped
        // by the host's cascade cancel (which walks SubAgentChildren).
        if !matches!(agent_status(world, parent), Some(AgentStatus::Waiting)) {
            world.entity_mut(parent).remove::<FanOutWaiting>();
            for late in here {
                starts::abandon(world, late);
            }
            continue;
        }
        // A `Waiting` parent from the query above still holds its `FanOutWaiting`
        // (only this system removes it, and each entity appears once per pass).
        let mut w = world
            .entity_mut(parent)
            .take::<FanOutWaiting>()
            .expect("a Waiting fan-out parent still holds FanOutWaiting");
        for start in here {
            starts::land(world, parent, &mut w, start);
        }
        let (mine, rest): (Vec<_>, Vec<_>) = handed.drain(..).partition(|h| h.parent == parent);
        handed = rest;
        for copy in mine {
            w.parts.extend(copy.parts);
            w.handing_up = w.handing_up.saturating_sub(1);
        }

        // 1. Reap workers that have reached a terminal state. A consumed
        // worker's result now lives in `w.summaries`/`w.failures`, so its heavy
        // components are dead weight - mark it for `slim_merged_workers`, which
        // drops them once the terminal snapshot has reached the persistence
        // lane. The entity itself stays (the host only despawns it when the
        // parent goes terminal), but without its context window, which would
        // otherwise stay resident for the whole remainder of the parent's run.
        let mut still_active = Vec::with_capacity(w.active.len());
        for aw in std::mem::take(&mut w.active) {
            match worker_terminal_result(world, aw.entity) {
                Some(result) => {
                    // Before the marker below hands this worker to
                    // `slim_merged_workers`, which drops its context window:
                    // after that its bibliography is only on disk.
                    merge_worker_sources(world, parent, aw.entity, &aw.item_id);
                    match result {
                        Ok(content) => {
                            // The copy is I/O: off the tick, landing on a
                            // later pass, unless the world has no runtime.
                            match io::HandUp::of(world, parent, &aw)
                                .map(|job| io::hand_up(world, job))
                            {
                                Some(Some(copied)) => w.parts.extend(copied.parts),
                                Some(None) => w.handing_up += 1,
                                None => {}
                            }
                            w.summaries.push((aw.item_id, content));
                        }
                        Err(message) => w.failures.push((aw.item_id, message)),
                    }
                    world.entity_mut(aw.entity).insert(MergedWorker);
                }
                None => still_active.push(aw),
            }
        }
        w.active = still_active;

        // 2. Start pending workers up to the concurrency cap - unless the
        // fan-out is paused, in which case the queue stays where it is. Reaping
        // above still runs: a worker that finished before the pause landed has a
        // result worth keeping.
        //
        // At most `MAX_WORKER_STARTS_PER_PASS` begun per pass. Each start is
        // prepared off the tick (see `starts`) and placed by a later pass
        // when it lands; the rest of the queue begins on the next pass.
        let mut started_this_pass = 0usize;
        let mut in_place = Vec::new();
        while !w.paused && w.max_workers.is_none_or(|cap| w.running() < cap) {
            if started_this_pass >= MAX_WORKER_STARTS_PER_PASS {
                break;
            }
            let Some(item) = w.pending.pop_front() else {
                break;
            };
            match starts::begin(world, parent, &w.config, &item, w.starting.len()) {
                Ok(start) => {
                    started_this_pass += 1;
                    w.starting.push(item.clone());
                    in_place.extend(starts::launch(world, parent, item.id, start));
                }
                Err(message) => w.failures.push((item.id, message)),
            }
        }
        // A world with no runtime ran its starts in place: they land now.
        for start in in_place {
            starts::land(world, parent, &mut w, start);
        }

        // 3. Finished when nothing is running, starting, queued or still
        // handing its files up.
        if w.running() == 0 && w.pending.is_empty() && w.handing_up == 0 {
            finish_fan_out(world, parent, w);
        } else {
            world.entity_mut(parent).insert(w);
        }
    }
}

/// A fan-out worker whose terminal result the parent has already consumed.
/// Set by [`fan_out_collect`]; consumed by [`slim_merged_workers`].
#[derive(Component)]
pub(crate) struct MergedWorker;

/// A finished worker whose context and spec were dropped from memory by
/// [`slim_merged_workers`]. What is left of it in the world is not its state,
/// so a read of its state goes to its run file instead.
#[derive(Component)]
pub(crate) struct Slimmed;

/// Drop a merged worker's heavy components once its terminal snapshot has
/// reached the persistence lane.
///
/// Ordering makes this safe on both sides: the marker is only set after the
/// parent consumed the worker's result (so the merge no longer reads the
/// worker), and the watermark gate (`PersistWatermark::persisted_status`)
/// holds the slim back until the terminal state is on its way to disk (so
/// nothing readable is lost - the entity's remaining metadata still identifies
/// the run, and its full final state is in the run dir).
pub(crate) fn slim_merged_workers(
    workers: Query<(Entity, &crate::pipeline::PersistWatermark), With<MergedWorker>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, watermark) in workers.iter() {
        crate::tick_scope::enter(entity);
        let terminal_persisted = matches!(
            watermark.persisted_status(),
            Some(
                leviath_core::run_meta::RunStatus::Complete
                    | leviath_core::run_meta::RunStatus::Error
                    | leviath_core::run_meta::RunStatus::Cancelled
            )
        );
        if !terminal_persisted {
            continue; // the terminal snapshot has not been dispatched yet
        }
        commands
            .entity(entity)
            .remove::<(ContextWindow, InferenceResult, RunSpecC, MergedWorker)>()
            .insert(Slimmed);
    }
}

/// Apply the failure policy, inject the consolidated report, and transition.
fn finish_fan_out(world: &mut World, parent: Entity, w: FanOutWaiting) {
    if !w.failures.is_empty() && w.config.on_worker_failure == WorkerFailure::FailAll {
        // Down the stage's `error` edge, which is what `WorkerFailure::FailAll`
        // has always been documented as doing. Writing the status alone made it a
        // dead run instead, so a blueprint with an `error_recovery` stage got the
        // recovery it declared only for provider failures, never for this.
        crate::pipeline::fail_stage_world(
            world,
            parent,
            format!(
                "fan_out: {} worker(s) failed (on_worker_failure = fail_all)",
                w.failures.len()
            ),
        );
        return;
    }

    // Everything above this line is common to both entry points; everything
    // below is the one thing that differs between them.
    match &w.origin {
        FanOutOrigin::Stage => finish_stage_fan_out(world, parent, &w),
        FanOutOrigin::Tool { call_id } => {
            let call_id = call_id.clone();
            finish_tool_fan_out(world, parent, &w, &call_id);
        }
    }
}

/// A fan-out stage: the report goes to the region the graph named and the
/// stage moves on to its `merge_stage`.
fn finish_stage_fan_out(world: &mut World, parent: Entity, w: &FanOutWaiting) {
    // Where the results land, and how much room they have there. A blueprint
    // that names a region of its own gets that region's budget to divide; the
    // default is the conversation region, which is also carrying the message
    // history.
    let region = w
        .config
        .results_region
        .as_ref()
        .map_or_else(|| "conversation".to_string(), ToString::to_string);
    let budget = world
        .get::<ContextWindow>(parent)
        .and_then(|window| window.get_region(&region).map(|r| r.max_tokens));
    let report = build_report(&w.summaries, &w.failures, budget);
    inject_results(world, parent, &region, &report, w.parts.clone());

    leave_fan_out(world, parent, &w.config);
}

/// A `fan_out` tool call: the report is that call's result, and the agent picks
/// up its stage where it left off.
///
/// Routed through the same path every other tool result takes, so the stage's
/// `tool_routing` decides where it lands - a region of its own, the conversation,
/// or (for a blueprint whose workers write files and whose parent does not need
/// to read their prose) somewhere it is cheaply dropped. That flexibility is not
/// a fan-out feature; it is the one every tool already has.
fn finish_tool_fan_out(world: &mut World, parent: Entity, w: &FanOutWaiting, call_id: &str) {
    // Sized against the region the result is actually routed to, so a report
    // headed for a big `sub_findings` is not trimmed to fit a conversation it
    // never enters.
    let region = routed_region(world, parent);
    let budget = world
        .get::<ContextWindow>(parent)
        .and_then(|window| window.get_region(&region).map(|r| r.max_tokens));
    let report = build_report(&w.summaries, &w.failures, budget);
    answer_call(world, parent, call_id, report);
}

/// The region a `fan_out` call's result lands in, by the stage's
/// `tool_routing`.
fn routed_region(world: &World, parent: Entity) -> String {
    world
        .get::<crate::components::ToolResultRoutingComponent>(parent)
        .map(|r| {
            r.routing
                .tool_regions
                .iter()
                .find(|(k, _)| {
                    leviath_tools::canonical_tool_name(k.as_str())
                        == leviath_core::stage_tools::FAN_OUT_TOOL
                })
                .map_or(&r.routing.default_region, |(_, v)| v)
                .to_string()
        })
        .unwrap_or_else(|| "conversation".to_string())
}

/// Answer a `fan_out` call with `text` and hand the agent back to its model.
///
/// Routed through the same path every other tool result takes. The report of
/// a finished fan-out arrives this way, and so does a refusal to start one.
fn answer_call(world: &mut World, parent: Entity, call_id: &str, text: String) {
    let routing = world
        .get::<crate::components::ToolResultRoutingComponent>(parent)
        .map(|r| r.routing.clone());
    let sensitivities = world
        .get::<crate::pipeline::ToolSensitivities>(parent)
        .map(|s| s.0.clone());
    if let Some(mut window) = world.get_mut::<ContextWindow>(parent) {
        crate::pipeline::apply_one_tool_result(
            &mut window,
            leviath_core::stage_tools::FAN_OUT_TOOL,
            call_id,
            text.into(),
            routing.as_ref(),
            sensitivities.as_ref(),
            // A report is already cut to its region's budget, and a refusal
            // is a line or two, so neither reaches the inline text ceiling.
            None,
        );
    }
    set_status(world, parent, AgentStatus::Active);
    world
        .entity_mut(parent)
        .insert(crate::pipeline::ReadyToInfer);
}

/// Ready the parent to run again and move it on: to the `merge_stage` when the
/// config names one, otherwise letting the fan-out stage's own transition
/// resolve.
///
/// Shared by the normal completion and by the never-terminal split failure, so
/// both leave the stage by the same door.
fn leave_fan_out(world: &mut World, parent: Entity, config: &FanOutDef) {
    set_status(world, parent, AgentStatus::Active);
    match config.merge_stage.as_ref().and_then(|name| {
        world
            .get::<RunSpecC>(parent)
            .and_then(|spec| spec.0.graph.stages.iter().position(|s| &s.name == name))
    }) {
        Some(idx) => crate::pipeline::force_transition(
            world,
            crate::world::AgentId::in_world(world, parent),
            idx,
        ),
        None => {
            world.entity_mut(parent).insert(ResolveTransition);
        }
    }
}

/// A worker's terminal result: `Some(Ok(deliverable))` if complete,
/// `Some(Err(reason))` if it errored/was cancelled/vanished, `None` if still
/// running.
///
/// A worker that called `submit_output` contributes exactly what it submitted.
/// Otherwise this falls back to the text of its last assistant message, which is
/// usually wrong: a worker whose final turn was a tool call has no trailing
/// text, so the merge stage gets an empty string, silently indistinguishable
/// from a worker that had nothing to say.
///
/// The fallback stays because it costs nothing and a graph that happens to
/// end on a text turn keeps working. A graph that wants the guarantee sets
/// `require_output` on its worker stage.
///
/// A worker whose stage set `require_output` and that finished without one is
/// reported as a **failure**, not as a success with empty content. It reached
/// `Complete` either way - the enforcement loop proceeds rather than stranding
/// the run, and a worker that burns its iterations against a validator it cannot
/// satisfy ends the same way. Counting that as success is how a fan-out reports
/// "10 succeeded, 0 failed" over ten empty sections, which is worse than an
/// error: the merge stage cannot tell an empty answer from a missing one, so it
/// writes a confident merge of nothing.
fn worker_terminal_result(world: &World, worker: Entity) -> Option<Result<String, String>> {
    match agent_status(world, worker) {
        None => Some(Err("worker vanished".to_string())),
        Some(AgentStatus::Complete) => {
            match world
                .get::<crate::persistence::FinalOutput>(worker)
                .map(|o| o.0.content.clone())
            {
                Some(content) => Some(Ok(content)),
                None if worker_requires_output(world, worker) => Some(Err(
                    "worker finished without the final output its stage requires".to_string(),
                )),
                None => Some(Ok(world
                    .get::<InferenceResult>(worker)
                    .map(|r| r.response.clone())
                    .unwrap_or_default())),
            }
        }
        Some(AgentStatus::Error { message }) => Some(Err(message)),
        Some(AgentStatus::Cancelled) => Some(Err("worker cancelled".to_string())),
        Some(_) => None,
    }
}

/// Whether the stage this worker is sitting in demands a final output.
fn worker_requires_output(world: &World, worker: Entity) -> bool {
    let Some(spec) = world.get::<RunSpecC>(worker) else {
        return false;
    };
    let Some(cursor) = world.get::<StageCursor>(worker) else {
        return false;
    };
    spec.0
        .graph
        .stages
        .get(cursor.index)
        .is_some_and(|s| s.require_output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::ToolResultRoutingComponent;
    use crate::pipeline::{ProcessResponse, ReadyToInfer, StageProgress, VisitCounts};
    use crate::spec::graph::{RunGraph, StageDef, StageMode as Mode};
    use crate::spec::names::{RegionName, StageName};
    use crate::test_graph::{both, layout, model, region, spec_c};
    use leviath_core::mime::Part;
    use leviath_core::output::Artifact;
    use leviath_core::{Region, RegionKind};
    use std::collections::HashSet;

    /// A spawner that spawns a trivial `Active` worker per item, refusing the ids
    /// in `fail`.
    pub(super) struct TestSpawner {
        fail: HashSet<String>,
    }

    impl TestSpawner {
        pub(super) fn ok() -> Arc<dyn FanOutSpawner> {
            Arc::new(TestSpawner {
                fail: HashSet::new(),
            })
        }
        pub(super) fn refusing(ids: &[&str]) -> Arc<dyn FanOutSpawner> {
            Arc::new(TestSpawner {
                fail: ids.iter().map(|s| s.to_string()).collect(),
            })
        }
    }

    impl FanOutSpawner for TestSpawner {
        fn prepare_worker(&self, request: SpawnRequest, _caller: Caller) -> WorkerPrep {
            let item_id = request.delivery.metadata[WORK_ITEM_LABEL].clone();
            let refused = self.fail.contains(&item_id);
            Box::pin(async move {
                if refused {
                    return Err(format!("spawn refused for '{item_id}'"));
                }
                Ok(Box::new(move |world: &mut World| test_worker(world, &item_id)) as PlaceWorker)
            })
        }

        fn find_worker(&self, query: &str) -> Result<BlueprintRef, String> {
            match query {
                "nobody" => Err(format!("no installed agent matches '{query}'")),
                _ => Ok(BlueprintRef::parse(query).expect("a valid name")),
            }
        }
    }

    /// A trivial `Active` worker for `item_id`, carrying the run metadata a
    /// real worker has.
    fn test_worker(world: &mut World, item_id: &str) -> Entity {
        world
            .spawn((
                AgentState {
                    agent_id: format!("worker-{item_id}"),
                    current_visit: String::new(),
                    current_stage: "w".to_string(),
                    iteration: 0,
                    status: AgentStatus::Active,
                    spawned_children_ids: vec![],
                    pending_wait: None,
                    accepts_messages: true,
                },
                // A real worker carries run metadata (attached by build_agent);
                // mirror that so the parent can record the worker's run-id.
                crate::persistence::RunMetadata {
                    run_id: format!("run-{item_id}"),
                    agent_name: "worker".to_string(),
                    agent_path: String::new(),
                    task: String::new(),
                    model: None,
                    workdir: String::new(),
                    num_stages: 1,
                    started_at: 0,
                    parent_run_id: None,
                    metadata: std::collections::HashMap::new(),
                    callback_url: None,
                    callback_secret: None,
                    title: None,
                    title_error: None,
                    blueprint_digest: None,
                    unattended: false,
                    yolo_profile: None,
                    read_paths: None,
                    output_request: None,
                    model_override: None,
                },
            ))
            .id()
    }

    pub(super) fn cfg(merge: Option<&str>, max_workers: u32, policy: WorkerFailure) -> FanOutDef {
        FanOutDef {
            worker: WorkerSource::Stage(StageName::new("w").unwrap()),
            merge_stage: merge.map(|m| StageName::new(m).unwrap()),
            max_workers: (max_workers > 0).then_some(max_workers),
            on_worker_failure: policy,
            split_prompt: "split".to_string(),
            results_region: None,
            max_items: None,
            max_attempts: None,
        }
    }

    fn window() -> ContextWindow {
        let mut w = ContextWindow::new(12_000);
        w.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::Clearable,
            10_000,
        ));
        w
    }

    /// The graph of [`fanout_blueprint`] over the default fan-out.
    fn fanout_graph() -> crate::spec::graph::RunGraph {
        let bp = fanout_blueprint(cfg(None, 3, WorkerFailure::Continue));
        both(bp).0.graph.clone()
    }

    /// A graph whose stage 0 is a fan-out stage and stage 1 is `merge`.
    pub(super) fn fanout_blueprint(config: FanOutDef) -> RunGraph {
        let layout = layout(
            vec![region("conversation", RegionKind::Clearable, 10_000)],
            12_000,
        );
        let s0 = StageDef {
            model: model("script", "m"),
            mode: Mode::FanOut(config),
            ..crate::test_graph::stage("fan")
        };
        let s1 = StageDef {
            model: model("script", "m"),
            ..crate::test_graph::stage("merge")
        };
        crate::test_graph::graph(vec![s0, s1], layout)
    }

    fn parent_state() -> AgentState {
        AgentState {
            agent_id: "parent".to_string(),
            current_visit: String::new(),
            current_stage: "fan".to_string(),
            iteration: 0,
            status: AgentStatus::Active,
            spawned_children_ids: vec![],
            pending_wait: None,
            accepts_messages: true,
        }
    }

    /// Spawn a parent sitting on `ProcessResponse` with `response` as its
    /// (split) inference output.
    pub(super) fn spawn_parent(world: &mut World, bp: RunGraph, response: &str) -> Entity {
        world
            .spawn((
                both(bp),
                StageCursor { index: 0 },
                parent_state(),
                StageProgress::default(),
                VisitCounts::default(),
                window(),
                InferenceResult {
                    attempt_id: String::new(),
                    response: response.to_string(),
                    tool_calls: vec![],
                    tokens_used: 0,
                    cut_off_at: None,
                    reasoning: None,
                    parts: Vec::new(),
                },
                ProcessResponse,
            ))
            .id()
    }

    /// Start one worker for `item` under `parent` and place it, as a pass of
    /// `fan_out_collect` does in a world with no runtime: in place.
    fn start_worker(
        world: &mut World,
        parent: Entity,
        config: &FanOutDef,
        item: &WorkItem,
    ) -> Result<Entity, String> {
        let start = starts::begin(world, parent, config, item, 0)?;
        let landed = starts::launch(world, parent, item.id.clone(), start)
            .expect("a world with no runtime runs a start in place");
        starts::place(world, parent, landed).1
    }

    pub(super) fn install(world: &mut World, spawner: Arc<dyn FanOutSpawner>) {
        world.insert_resource(FanOutSpawnerRes(spawner));
    }

    /// A spawner that records where each worker's graph comes from, then
    /// spawns it as [`TestSpawner`] does.
    struct Recording {
        sources: std::sync::Mutex<Vec<SpawnSource>>,
    }

    impl FanOutSpawner for Recording {
        fn prepare_worker(&self, request: SpawnRequest, caller: Caller) -> WorkerPrep {
            self.sources.lock().unwrap().push(request.source.clone());
            TestSpawner {
                fail: HashSet::new(),
            }
            .prepare_worker(request, caller)
        }

        fn find_worker(&self, query: &str) -> Result<BlueprintRef, String> {
            BlueprintRef::parse(query).map_err(|e| e.to_string())
        }
    }

    /// A same-graph worker runs the blueprint its parent ran, so the files
    /// beside it are there for the worker too; a parent that ran its own
    /// graph hands the worker that graph.
    #[test]
    fn a_same_graph_worker_runs_what_its_parent_ran() {
        let mut world = World::new();
        let recording = Arc::new(Recording {
            sources: std::sync::Mutex::new(Vec::new()),
        });
        install(&mut world, recording.clone());
        let config = cfg(None, 2, WorkerFailure::Continue);
        let parent = spawn_parent(&mut world, fanout_blueprint(config.clone()), "");

        start_worker(&mut world, parent, &config, &item("a")).expect("the worker starts");
        let mut raw = (*world.get::<RunSpecC>(parent).unwrap().0).clone();
        raw.origin = crate::spec::run_spec::SpecOrigin::Raw;
        let graph = raw.graph.clone();
        world
            .entity_mut(parent)
            .insert(RunSpecC(Arc::new(raw.clone())));
        start_worker(&mut world, parent, &config, &item("b")).expect("the worker starts");
        let path = crate::spec::names::BlueprintPath::new(
            std::env::temp_dir().join("t").to_string_lossy(),
        )
        .unwrap();
        raw.origin = crate::spec::run_spec::SpecOrigin::BlueprintFile {
            path: path.clone(),
            name: crate::spec::names::BlueprintName::new("t").unwrap(),
            digest: None,
            version: String::new(),
        };
        world.entity_mut(parent).insert(RunSpecC(Arc::new(raw)));
        start_worker(&mut world, parent, &config, &item("c")).expect("the worker starts");

        let sources = recording.sources.lock().unwrap();
        assert_eq!(
            sources[0],
            SpawnSource::Blueprint(BlueprintRef::parse("t").unwrap())
        );
        assert_eq!(sources[1], SpawnSource::Raw(Box::new(graph)));
        assert_eq!(sources[2], SpawnSource::BlueprintFile(path));
        assert!(recording.find_worker("helper").is_ok());
        assert!(recording.find_worker("helper@not-a-digest").is_err());
    }

    pub(super) fn status_of(world: &World, e: Entity) -> AgentStatus {
        world.get::<AgentState>(e).unwrap().status.clone()
    }

    /// Assert an agent is in an `Error` state (by discriminant, so no unmatched
    /// `matches!` arm is left uncovered).
    fn assert_errored(world: &World, e: Entity) {
        assert_eq!(
            std::mem::discriminant(&status_of(world, e)),
            std::mem::discriminant(&AgentStatus::Error {
                message: String::new()
            })
        );
    }

    pub(super) fn conversation_text(world: &World, e: Entity) -> String {
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .content
            .iter()
            .map(|entry| entry.content.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Stand in for the dispatcher: take the work items the fixture put in the
    /// parent's pending response and park it, exactly as an accepted `fan_out`
    /// call does. Lets the collector's tests state their items as JSON, which is
    /// how a model states them.
    fn split(world: &mut World, e: Entity) {
        let items: Vec<WorkItem> =
            serde_json::from_str(&world.get::<InferenceResult>(e).unwrap().response)
                .expect("the fixture's response is a work-item array");
        let config = stage_config(world, e);
        world
            .entity_mut(e)
            .remove::<ProcessResponse>()
            .remove::<InferenceResult>();
        begin_fan_out(world, e, config, items, FanOutOrigin::Stage);
    }

    /// The fan-out config off the fixture's blueprint.
    ///
    /// Walked in reverse so the fixture's ordinary `merge` stage is visited
    /// first: the non-fan-out arm is then a branch the suite actually takes,
    /// rather than one that only exists to satisfy the match.
    fn stage_config(world: &World, e: Entity) -> FanOutDef {
        world
            .get::<RunSpecC>(e)
            .unwrap()
            .0
            .graph
            .stages
            .iter()
            .rev()
            .find_map(|stage| match &stage.mode {
                StageMode::FanOut(def) => Some(def.clone()),
                _ => None,
            })
            .expect("the fixture has a fan-out stage")
    }

    /// A work item with the given id and no inputs.
    pub(super) fn item(id: &str) -> WorkItem {
        WorkItem {
            id: id.to_string(),
            inputs: Default::default(),
        }
    }

    fn complete_worker(world: &mut World, worker: Entity, content: &str) {
        set_status(world, worker, AgentStatus::Complete);
        world.entity_mut(worker).insert(InferenceResult {
            attempt_id: String::new(),
            response: content.to_string(),
            tool_calls: vec![],
            tokens_used: 0,
            cut_off_at: None,
            reasoning: None,
            parts: Vec::new(),
        });
    }

    // ── frame_split_round ─────────────────────────────────────────────────────

    /// Run the framing system over a parent that has just entered its stage.
    fn run_framing(world: &mut World, e: Entity, visits: &[(&str, usize)]) -> String {
        let mut counts = VisitCounts::default();
        for (name, n) in visits {
            counts.0.insert((*name).to_string(), *n);
        }
        world
            .entity_mut(e)
            .insert(counts)
            .insert(crate::pipeline::StageJustEntered {
                index: 0,
                name: "fan".to_string(),
            });
        let mut schedule = Schedule::default();
        schedule.add_systems(frame_split_round);
        schedule.run(world);
        conversation_text(world, e)
    }

    /// A stage entered once has nothing to be told, and pays nothing for the
    /// mechanism: the ordinary single-fan-out run is untouched.
    #[test]
    fn a_first_split_round_is_not_framed() {
        let mut world = World::new();
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailure::Continue)),
            "",
        );

        let convo = run_framing(&mut world, e, &[("fan", 1)]);

        assert_eq!(convo, "", "nothing injected on a first entry");
    }

    /// The failure this exists for. On a re-entry the model is asked a different
    /// question from the one it already answered, told what came back, and told
    /// that an empty list is a real reply.
    #[test]
    fn a_repeat_split_round_is_framed_with_what_was_already_researched() {
        let mut world = World::new();
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailure::Continue)),
            "",
        );
        world.entity_mut(e).insert(PreviousWorkItems(vec![
            "glp1-mechanism".to_string(),
            "post-cessation-regain".to_string(),
        ]));

        let convo = run_framing(&mut world, e, &[("fan", 2)]);

        assert!(convo.contains("split round 2"), "{convo}");
        assert!(convo.contains("glp1-mechanism"), "{convo}");
        assert!(convo.contains("post-cessation-regain"), "{convo}");
        assert!(convo.contains("still unanswered"), "{convo}");
        assert!(
            convo.contains("empty `items` array"),
            "and that finishing is sayable: {convo}"
        );
    }

    /// The ids live on a component, and a daemon restart between the two entries
    /// drops it. The framing still fires, because "you have been here before" is
    /// the part that matters.
    #[test]
    fn a_repeat_round_is_framed_even_with_the_previous_ids_lost() {
        let mut world = World::new();
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailure::Continue)),
            "",
        );

        let convo = run_framing(&mut world, e, &[("fan", 3)]);

        assert!(convo.contains("split round 3"), "{convo}");
        assert!(convo.contains("already been handed out"), "{convo}");
    }

    /// A wide fan-out's ids are listed up to a bound, so the instruction after
    /// them is not buried under thirty slugs.
    #[test]
    fn a_wide_previous_round_lists_a_bounded_number_of_ids() {
        let mut world = World::new();
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailure::Continue)),
            "",
        );
        let ids: Vec<String> = (0..20).map(|i| format!("item-{i}")).collect();
        world.entity_mut(e).insert(PreviousWorkItems(ids));

        let convo = run_framing(&mut world, e, &[("fan", 2)]);

        assert!(convo.contains("item-0"), "{convo}");
        assert!(!convo.contains("item-19"), "{convo}");
        assert!(convo.contains("and 8 more"), "{convo}");
    }

    /// Only fan-out stages are framed; every other stage entry is untouched.
    #[test]
    fn a_stage_that_is_not_a_fan_out_is_not_framed() {
        let mut world = World::new();
        let mut bp = fanout_blueprint(cfg(None, 2, WorkerFailure::Continue));
        bp.stages[0].mode = Mode::Autonomous;
        let e = spawn_parent(&mut world, bp, "");

        let convo = run_framing(&mut world, e, &[("fan", 2)]);

        assert_eq!(convo, "");
    }

    /// Starting a fan-out records what it handed out, which is what the next
    /// round is framed with.
    #[test]
    fn starting_a_fan_out_records_its_work_item_ids() {
        let mut world = World::new();
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailure::Continue)),
            "",
        );

        begin_fan_out(
            &mut world,
            e,
            cfg(None, 2, WorkerFailure::Continue),
            vec![item("a"), item("b")],
            FanOutOrigin::Stage,
        );

        assert_eq!(
            world.get::<PreviousWorkItems>(e),
            Some(&PreviousWorkItems(vec!["a".to_string(), "b".to_string()]))
        );
    }

    // ── parse_fan_out_call ────────────────────────────────────────────────────

    /// The arguments came through a schema the provider enforced, so the reader
    /// is strict: it takes the shape asked for and names anything else.
    #[test]
    fn parse_fan_out_call_reads_the_whole_shape() {
        let request = parse_fan_out_call(&serde_json::json!({
            "agent": "researcher",
            "max_workers": 4,
            "items": [
                {"id": "a", "inputs": {"task": "q1"}},
                {"id": "b", "inputs": {"task": "q2"}}
            ]
        }))
        .expect("parses");
        assert_eq!(request.agent.as_deref(), Some("researcher"));
        assert_eq!(request.max_workers, Some(4));
        assert_eq!(request.items.len(), 2);
        assert_eq!(
            request.items[1].inputs["task"],
            crate::spec::inputs::RawInput::Text("q2".into())
        );
    }

    /// An empty list is a real answer, not a malformed call: it means there is
    /// nothing to hand out.
    #[test]
    fn parse_fan_out_call_accepts_an_empty_list() {
        let request = parse_fan_out_call(&serde_json::json!({"items": []})).expect("parses");
        assert!(request.items.is_empty());
        assert_eq!(request.agent, None);
        assert_eq!(request.max_workers, None);
    }

    /// A call asking for no workers at all is refused, with how to ask for no
    /// cap instead: leave `max_workers` out.
    #[test]
    fn parse_fan_out_call_refuses_zero_workers() {
        let err = parse_fan_out_call(&serde_json::json!({"items": [], "max_workers": 0}))
            .expect_err("zero workers is not a cap");
        assert_eq!(
            err,
            "fan_out `max_workers` must be at least 1; leave it out to run every item at once"
        );
    }

    /// A blank agent is the same as none: a fan-out stage names its worker in
    /// the blueprint, and an empty string would otherwise override it with
    /// nothing.
    #[test]
    fn parse_fan_out_call_ignores_a_blank_agent() {
        let request =
            parse_fan_out_call(&serde_json::json!({"agent": "  ", "items": []})).expect("parses");
        assert_eq!(request.agent, None);
    }

    /// Every rejection names what was wrong, because the model reads it and
    /// corrects on its next turn.
    #[test]
    fn parse_fan_out_call_names_what_was_wrong() {
        let cases = [
            (serde_json::json!("nope"), "must be an object"),
            (serde_json::json!({}), "requires an `items` array"),
            (serde_json::json!({"items": "all"}), "must be an array"),
            (
                serde_json::json!({"items": [{"id": 4}]}),
                "not {id, inputs}",
            ),
        ];
        for (args, expected) in cases {
            let err = parse_fan_out_call(&args).unwrap_err();
            assert!(err.contains(expected), "{args}: {err}");
        }
    }

    // ── begin_fan_out / start_pending_fan_outs ────────────────────────────────

    /// The one way in: the parent parks on its workers and records what it
    /// handed out.
    #[test]
    fn begin_fan_out_parks_the_parent_on_its_workers() {
        let mut world = World::new();
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailure::Continue)),
            "",
        );

        begin_fan_out(
            &mut world,
            e,
            cfg(None, 2, WorkerFailure::Continue),
            vec![item("a"), item("b")],
            FanOutOrigin::Stage,
        );

        assert_eq!(status_of(&world, e), AgentStatus::Waiting);
        assert_eq!(
            world.get::<FanOutWaiting>(e).expect("parked").pending.len(),
            2
        );
        assert!(world.get::<FannedOut>(e).is_some());
        assert_eq!(
            world.get::<PreviousWorkItems>(e),
            Some(&PreviousWorkItems(vec!["a".to_string(), "b".to_string()]))
        );
    }

    /// `max_items` is a ceiling on the work, not just on concurrency.
    #[test]
    fn begin_fan_out_keeps_only_the_first_max_items() {
        let mut world = World::new();
        let mut config = cfg(None, 2, WorkerFailure::Continue);
        config.max_items = Some(3);
        let e = spawn_parent(&mut world, fanout_blueprint(config.clone()), "");

        let items: Vec<WorkItem> = (0..10).map(|i| item(&format!("w{i}"))).collect();
        begin_fan_out(&mut world, e, config, items, FanOutOrigin::Stage);

        let w = world.get::<FanOutWaiting>(e).expect("parked");
        let kept: Vec<&str> = w.pending.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(kept, ["w0", "w1", "w2"]);
    }

    /// An empty fan-out is not a special case: it parks with nothing pending and
    /// the collector finishes it on the next tick.
    #[test]
    fn an_empty_fan_out_finishes_through_the_ordinary_path() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailure::Continue)),
            "",
        );

        begin_fan_out(
            &mut world,
            e,
            cfg(Some("merge"), 2, WorkerFailure::Continue),
            Vec::new(),
            FanOutOrigin::Stage,
        );
        fan_out_collect(&mut world);

        assert_eq!(status_of(&world, e), AgentStatus::Active);
        assert_eq!(
            world.get::<StageCursor>(e).map(|c| c.index),
            Some(1),
            "straight through to the merge stage"
        );
    }

    /// The dispatcher hands the call over as a component; this is the tick that
    /// turns it into running workers.
    #[test]
    fn start_pending_fan_outs_starts_what_the_dispatcher_accepted() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        // An ordinary stage, so this is the tool door: a fan-out stage's own
        // call is a stage fan-out and is covered separately.
        let mut bp = fanout_blueprint(cfg(None, 2, WorkerFailure::Continue));
        bp.stages[0].mode = Mode::Autonomous;
        let e = spawn_parent(&mut world, bp, "");
        world.entity_mut(e).insert(PendingFanOut {
            call_id: "call-1".to_string(),
            request: parse_fan_out_call(&serde_json::json!({
                "agent": "researcher",
                "items": [{"id": "a"}]
            }))
            .unwrap(),
        });

        start_pending_fan_outs(&mut world);

        assert!(world.get::<PendingFanOut>(e).is_none(), "consumed");
        let w = world.get::<FanOutWaiting>(e).expect("parked");
        assert_eq!(w.pending.len(), 1);
        assert_eq!(
            w.origin,
            FanOutOrigin::Tool {
                call_id: "call-1".to_string()
            }
        );
        assert_eq!(
            w.config.worker,
            WorkerSource::Blueprint(BlueprintRef::parse("researcher").unwrap()),
            "the call named its worker"
        );
    }

    /// A `mode = "fan_out"` stage's call comes back through the STAGE door, not
    /// the tool one: its report goes to `results_region` and it moves on to
    /// `merge_stage`.
    ///
    /// The call itself is identical either way, so only the stage can say which
    /// this is - and getting it wrong made `results_region` and `merge_stage`
    /// dead config. A live `deep-researcher` fan-out delivered three workers'
    /// findings into `conversation` as a tool result and resumed the split stage
    /// instead of writing `sub_findings` and moving to `analyze`. Every unit test
    /// passed, because they all built the origin by hand and never came through
    /// `start_pending_fan_outs`.
    #[test]
    fn a_fan_out_stages_call_is_delivered_as_a_stage_not_a_tool_result() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let mut config = cfg(Some("merge"), 2, WorkerFailure::Continue);
        config.results_region = Some(RegionName::new("sub_findings").unwrap());
        let e = spawn_parent(&mut world, fanout_blueprint(config), "");
        world
            .get_mut::<ContextWindow>(e)
            .unwrap()
            .add_region(Region::new(
                "sub_findings".to_string(),
                RegionKind::Pinned,
                4_000,
            ));
        world.entity_mut(e).insert(PendingFanOut {
            call_id: "call-1".to_string(),
            request: parse_fan_out_call(&serde_json::json!({
                "items": [{"id": "a"}]
            }))
            .unwrap(),
        });

        start_pending_fan_outs(&mut world);
        assert_eq!(
            world.get::<FanOutWaiting>(e).expect("parked").origin,
            FanOutOrigin::Stage,
            "a fan-out stage's own call is a stage fan-out"
        );

        fan_out_collect(&mut world);
        let worker = world.get::<SubAgentChildren>(e).expect("linked").children[0];
        complete_worker(&mut world, worker, "what the worker found");
        fan_out_collect(&mut world);

        let findings = world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("sub_findings")
            .expect("the results region")
            .content
            .iter()
            .map(|entry| entry.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            findings.contains("what the worker found"),
            "the report goes to results_region: {findings}"
        );
        assert_eq!(
            world.get::<StageCursor>(e).map(|c| c.index),
            Some(1),
            "and the stage moves on to its merge stage"
        );
    }

    /// A tool call made inside a fan-out stage still picks up that stage's own
    /// keys, so `mode = \"fan_out\"` really is sugar over the same engine.
    #[test]
    fn a_call_inside_a_fan_out_stage_inherits_the_stages_keys() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let mut config = cfg(Some("merge"), 7, WorkerFailure::Continue);
        config.max_items = Some(2);
        let e = spawn_parent(&mut world, fanout_blueprint(config), "");
        world.entity_mut(e).insert(PendingFanOut {
            call_id: "call-1".to_string(),
            request: parse_fan_out_call(&serde_json::json!({
                "items": [{"id": "a"}]
            }))
            .unwrap(),
        });

        start_pending_fan_outs(&mut world);

        let w = world.get::<FanOutWaiting>(e).expect("parked");
        assert_eq!(
            w.config.merge_stage.as_ref().map(StageName::as_str),
            Some("merge")
        );
        assert_eq!(w.config.max_items, Some(2));
        assert_eq!(w.max_workers, Some(7));
    }

    /// The tool is recognised by name and nothing else is.
    #[test]
    fn the_fan_out_tool_is_recognised_by_name() {
        assert!(is_fan_out_tool("fan_out"));
        assert!(!is_fan_out_tool("spawn_agent"));
    }

    /// The headline case: an ordinary stage grants the tool and fans out in the
    /// middle of its own work. There is no stage config to inherit, so the call
    /// brings everything.
    #[test]
    fn an_ordinary_stage_can_fan_out_mid_work() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let mut bp = fanout_blueprint(cfg(None, 2, WorkerFailure::Continue));
        bp.stages[0].mode = Mode::Autonomous;
        let e = spawn_parent(&mut world, bp, "");
        world.entity_mut(e).insert(PendingFanOut {
            call_id: "call-1".to_string(),
            request: parse_fan_out_call(&serde_json::json!({
                "agent": "researcher",
                "max_workers": 3,
                "items": [{"id": "a"}]
            }))
            .unwrap(),
        });

        start_pending_fan_outs(&mut world);

        let w = world.get::<FanOutWaiting>(e).expect("parked");
        assert_eq!(
            w.config.worker,
            WorkerSource::Blueprint(BlueprintRef::parse("researcher").unwrap())
        );
        assert_eq!(w.max_workers, Some(3));
        assert_eq!(
            w.config.merge_stage, None,
            "an ordinary stage has no merge stage to fall into"
        );
    }

    /// A stage that names a `results_region` gets its report there, not in the
    /// conversation. This is the region the merge stage is told to read, so a
    /// report that misses it is a fan-out whose findings nobody sees.
    #[test]
    fn a_stage_fan_out_writes_to_its_results_region() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let mut config = cfg(Some("merge"), 2, WorkerFailure::Continue);
        config.results_region = Some(RegionName::new("sub_findings").unwrap());
        let e = spawn_parent(&mut world, fanout_blueprint(config.clone()), "");
        world
            .get_mut::<ContextWindow>(e)
            .unwrap()
            .add_region(Region::new(
                "sub_findings".to_string(),
                RegionKind::Pinned,
                4_000,
            ));
        begin_fan_out(&mut world, e, config, vec![item("a")], FanOutOrigin::Stage);

        fan_out_collect(&mut world);
        let worker = world.get::<SubAgentChildren>(e).expect("linked").children[0];
        complete_worker(&mut world, worker, "what the worker found");
        fan_out_collect(&mut world);

        let region = world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("sub_findings")
            .expect("the results region")
            .content
            .iter()
            .map(|entry| entry.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(region.contains("what the worker found"), "{region}");
    }

    /// A fan-out whose stage has already spent its `max_iterations` must still
    /// deliver. The nudges that get a reluctant model to call the tool are
    /// inferences, so they spend the stage's budget: `deep-researcher` allows
    /// `investigate` four, a live run spent three answering in prose and the
    /// fourth calling `fan_out`, and three workers then researched for thirteen
    /// minutes. Discarding that because the split took four tries is the same
    /// failure - finished work thrown away - that this whole path exists to stop.
    #[test]
    fn a_fan_out_still_merges_when_its_stage_is_out_of_iterations() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let mut bp = fanout_blueprint(cfg(Some("merge"), 2, WorkerFailure::Continue));
        bp.stages[0].max_iterations = Some(4);
        let e = spawn_parent(&mut world, bp, "");
        // The stage is at its cap, exactly as it is when the fan-out starts on
        // the last iteration the stage had.
        world.entity_mut(e).insert(StageProgress {
            iterations: 4,
            ..Default::default()
        });
        begin_fan_out(
            &mut world,
            e,
            cfg(Some("merge"), 2, WorkerFailure::Continue),
            vec![item("a")],
            FanOutOrigin::Stage,
        );

        fan_out_collect(&mut world); // starts the worker
        let worker = world.get::<SubAgentChildren>(e).expect("linked").children[0];
        complete_worker(&mut world, worker, "what the worker found");
        fan_out_collect(&mut world); // reaps and merges

        assert_eq!(
            world.get::<StageCursor>(e).map(|c| c.index),
            Some(1),
            "the merge stage is entered, not skipped"
        );
        let convo = conversation_text(&world, e);
        assert!(
            convo.contains("what the worker found"),
            "and the findings survive: {convo}"
        );
    }

    // ── the tool origin's delivery ────────────────────────────────────────────

    /// A fan-out started by a tool call comes back as that call's result and the
    /// agent picks its stage up where it left off - it does not transition.
    #[test]
    fn a_tool_fan_out_returns_its_report_as_the_calls_result() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailure::Continue)),
            "",
        );
        begin_fan_out(
            &mut world,
            e,
            cfg(Some("merge"), 2, WorkerFailure::Continue),
            vec![item("a")],
            FanOutOrigin::Tool {
                call_id: "call-1".to_string(),
            },
        );

        fan_out_collect(&mut world); // starts the worker
        let worker = world.get::<SubAgentChildren>(e).expect("linked").children[0];
        complete_worker(&mut world, worker, "what the worker found");
        fan_out_collect(&mut world); // reaps and delivers

        assert_eq!(status_of(&world, e), AgentStatus::Active);
        assert!(
            world.get::<crate::pipeline::ReadyToInfer>(e).is_some(),
            "back in front of the model, not transitioning"
        );
        assert_eq!(
            world.get::<StageCursor>(e).map(|c| c.index),
            Some(0),
            "still in the stage that called it"
        );
        let convo = conversation_text(&world, e);
        assert!(convo.contains("what the worker found"), "{convo}");
    }

    /// No context window means nowhere to put the report, and the agent is still
    /// handed back to the model rather than left parked. The same shape
    /// `inject_results` takes for the stage origin.
    #[test]
    fn a_tool_fan_out_without_a_window_still_resumes_the_agent() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = world
            .spawn((
                both(fanout_blueprint(cfg(None, 2, WorkerFailure::Continue))),
                StageCursor { index: 0 },
                parent_state(),
                StageProgress::default(),
                VisitCounts::default(),
            ))
            .id();
        begin_fan_out(
            &mut world,
            e,
            cfg(None, 2, WorkerFailure::Continue),
            Vec::new(),
            FanOutOrigin::Tool {
                call_id: "call-1".to_string(),
            },
        );

        fan_out_collect(&mut world);

        assert_eq!(status_of(&world, e), AgentStatus::Active);
        assert!(world.get::<crate::pipeline::ReadyToInfer>(e).is_some());
    }

    /// Routing that says nothing about `fan_out` sends the report to whatever
    /// the stage's default region is, like any other unlisted tool.
    #[test]
    fn a_tool_fan_out_falls_back_to_the_stages_default_region() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailure::Continue)),
            "",
        );
        world
            .get_mut::<ContextWindow>(e)
            .unwrap()
            .add_region(Region::new(
                "notes".to_string(),
                RegionKind::Clearable,
                4_000,
            ));
        world
            .entity_mut(e)
            .insert(crate::components::ToolResultRoutingComponent {
                routing: crate::spec::graph::ToolRoutingDef {
                    default_region: RegionName::new("notes").unwrap(),
                    tool_regions: std::collections::BTreeMap::from([(
                        crate::spec::names::ToolName::new("read_file").unwrap(),
                        RegionName::new("sources").unwrap(),
                    )]),
                    max_result_tokens: None,
                    tool_max_result_tokens: std::collections::BTreeMap::new(),
                    keep_results: true,
                },
            })
            // A declared sensitivity travels with the result, as it does for
            // every other tool.
            .insert(crate::pipeline::ToolSensitivities(
                std::collections::HashMap::from([(
                    "fan_out".to_string(),
                    leviath_core::TaintLevel::Public,
                )]),
            ));
        begin_fan_out(
            &mut world,
            e,
            cfg(None, 2, WorkerFailure::Continue),
            vec![item("a")],
            FanOutOrigin::Tool {
                call_id: "call-1".to_string(),
            },
        );

        fan_out_collect(&mut world);
        let worker = world.get::<SubAgentChildren>(e).expect("linked").children[0];
        complete_worker(&mut world, worker, "default-routed finding");
        fan_out_collect(&mut world);

        let notes = world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("notes")
            .expect("the default region")
            .content
            .iter()
            .map(|entry| entry.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(notes.contains("default-routed finding"), "{notes}");
    }

    /// The report is routed like any other tool result, so a blueprint that
    /// sends `fan_out` somewhere of its own gets it there.
    #[test]
    fn a_tool_fan_outs_report_follows_the_stages_routing() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailure::Continue)),
            "",
        );
        // The region the routing points at has to exist for the result to land
        // in it; a routing rule to a region the stage does not carry is a
        // blueprint `lev validate` refuses.
        world
            .get_mut::<ContextWindow>(e)
            .unwrap()
            .add_region(Region::new(
                "findings".to_string(),
                RegionKind::Clearable,
                4_000,
            ));
        world
            .entity_mut(e)
            .insert(crate::components::ToolResultRoutingComponent {
                routing: crate::spec::graph::ToolRoutingDef {
                    default_region: RegionName::new("conversation").unwrap(),
                    // A second rule that does not match, so the lookup has
                    // something to reject as well as something to find.
                    tool_regions: std::collections::BTreeMap::from([
                        (
                            crate::spec::names::ToolName::new("fan_out").unwrap(),
                            RegionName::new("findings").unwrap(),
                        ),
                        (
                            crate::spec::names::ToolName::new("read_file").unwrap(),
                            RegionName::new("sources").unwrap(),
                        ),
                    ]),
                    max_result_tokens: None,
                    tool_max_result_tokens: std::collections::BTreeMap::new(),
                    keep_results: true,
                },
            });
        begin_fan_out(
            &mut world,
            e,
            cfg(None, 2, WorkerFailure::Continue),
            vec![item("a")],
            FanOutOrigin::Tool {
                call_id: "call-1".to_string(),
            },
        );

        fan_out_collect(&mut world);
        let worker = world.get::<SubAgentChildren>(e).expect("linked").children[0];
        complete_worker(&mut world, worker, "routed finding");
        fan_out_collect(&mut world);

        let findings = world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("findings")
            .expect("the routed region")
            .content
            .iter()
            .map(|entry| entry.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(findings.contains("routed finding"), "{findings}");
    }

    // ── fan_out_collect: worker lifecycle + merge ─────────────────────────────

    /// A paused fan-out does not start the next queued worker.
    ///
    /// Without the latch, pausing a parent pauses the children that are running
    /// and the collector immediately launches their replacements out of
    /// `pending` - so the run keeps spending money and the pause achieves
    /// nothing visible.
    #[test]
    fn a_paused_fan_out_starts_no_further_workers() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        // Cap of one against three items, so there is always something queued.
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 1, WorkerFailure::Continue)),
            "",
        );
        begin_fan_out(
            &mut world,
            e,
            cfg(Some("merge"), 1, WorkerFailure::Continue),
            vec![item("a"), item("b"), item("c")],
            FanOutOrigin::Stage,
        );
        fan_out_collect(&mut world);
        assert_eq!(
            world.get::<SubAgentChildren>(e).unwrap().children.len(),
            1,
            "one worker runs under a cap of one"
        );

        world
            .get_mut::<FanOutWaiting>(e)
            .expect("parked")
            .set_paused(true);
        // Finish the running worker: its slot frees, which is exactly when the
        // collector would otherwise reach into the queue.
        let running = world.get::<SubAgentChildren>(e).unwrap().children[0];
        complete_worker(&mut world, running, "done");
        fan_out_collect(&mut world);

        assert_eq!(
            world.get::<SubAgentChildren>(e).unwrap().children.len(),
            1,
            "the freed slot stays empty while the fan-out is paused"
        );
        let w = world.get::<FanOutWaiting>(e).expect("still parked");
        assert_eq!(w.pending.len(), 2, "the queue is held, not consumed");
        assert_eq!(
            w.summaries.len(),
            1,
            "the worker that finished before the pause is still reaped"
        );

        // Releasing the latch lets the queue move again.
        world
            .get_mut::<FanOutWaiting>(e)
            .expect("parked")
            .set_paused(false);
        fan_out_collect(&mut world);
        assert_eq!(
            world.get::<SubAgentChildren>(e).unwrap().children.len(),
            2,
            "resuming starts the next queued worker"
        );
    }

    /// The latch survives a daemon restart: it rides the persisted fan-out state
    /// like everything else the parent is parked on.
    #[test]
    fn the_fan_out_pause_round_trips_through_its_persisted_state() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 1, WorkerFailure::Continue)),
            r#"[{"id":"a"},{"id":"b"}]"#,
        );
        split(&mut world, e);
        world
            .get_mut::<FanOutWaiting>(e)
            .expect("parked")
            .set_paused(true);

        let state = world.get::<FanOutWaiting>(e).expect("parked").to_state();
        assert!(state.paused, "the latch is written out");

        restore_fan_out_waiting(&mut world, e, state, &|_| None);
        assert!(
            world.get::<FanOutWaiting>(e).expect("parked").is_paused(),
            "and comes back paused, so a restart does not quietly resume the fan-out"
        );
    }

    #[test]
    fn collect_starts_workers_then_merges_on_completion() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailure::Continue)),
            r#"[{"id":"a"},{"id":"b"}]"#,
        );
        split(&mut world, e);
        fan_out_collect(&mut world);
        // Two workers started and tracked.
        let kids = world.get::<SubAgentChildren>(e).unwrap().children.clone();
        assert_eq!(kids.len(), 2);
        assert!(world.get::<FanOutWaiting>(e).is_some());
        // Each worker got a ParentRef at depth 1.
        for k in &kids {
            assert_eq!(world.get::<ParentRef>(*k).unwrap().depth, 1);
        }

        // Complete both workers, then collect merges to the merge stage.
        for k in &kids {
            complete_worker(&mut world, *k, "fixed it");
        }
        fan_out_collect(&mut world);
        assert!(world.get::<FanOutWaiting>(e).is_none());
        assert_eq!(status_of(&world, e), AgentStatus::Active);
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
        assert!(world.get::<ReadyToInfer>(e).is_some());
        // The consolidated report landed in the parent's conversation.
        assert!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .current_tokens
                > 0
        );
    }

    /// Run the slim system once over `world`.
    fn run_slim(world: &mut World) {
        let mut schedule = bevy_ecs::schedule::Schedule::default();
        schedule.add_systems(slim_merged_workers);
        schedule.run(world);
    }

    /// A merged worker keeps its heavy components until its terminal snapshot
    /// has been dispatched, then sheds them, so a finished worker does not hold
    /// a full context window resident until the parent goes terminal.
    #[test]
    fn merged_workers_are_slimmed_once_their_terminal_state_is_persisted() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailure::Continue)),
            r#"[{"id":"a"}]"#,
        );
        split(&mut world, e);
        fan_out_collect(&mut world);
        let worker = world.get::<SubAgentChildren>(e).unwrap().children[0];
        // Give the worker a context window so there is something to shed.
        world
            .entity_mut(worker)
            .insert((window(), crate::pipeline::PersistWatermark::default()));
        complete_worker(&mut world, worker, "done");
        fan_out_collect(&mut world);

        // Consumed by the merge and marked - but its terminal snapshot has not
        // been dispatched, so it keeps its state.
        assert!(world.get::<MergedWorker>(worker).is_some());
        run_slim(&mut world);
        assert!(
            world.get::<ContextWindow>(worker).is_some(),
            "unpersisted terminal state stays resident"
        );

        // Stamp the watermark terminal, and the worker sheds its heavy parts.
        let mut wm = crate::pipeline::PersistWatermark::default();
        wm.stamp_status(leviath_core::run_meta::RunStatus::Complete);
        world.entity_mut(worker).insert(wm);
        run_slim(&mut world);
        assert!(world.get::<ContextWindow>(worker).is_none());
        assert!(world.get::<MergedWorker>(worker).is_none());
        // The entity itself survives for the host's bookkeeping, but what is
        // left of it is not its state: that is read from its file.
        assert!(world.get::<AgentState>(worker).is_some());
        assert!(crate::state::inspect::inspect(&world, worker).is_none());
    }

    #[test]
    fn collect_respects_max_workers_and_stages_pending() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 1, WorkerFailure::Continue)),
            r#"[{"id":"a"},{"id":"b"}]"#,
        );
        split(&mut world, e);
        fan_out_collect(&mut world);
        // Only one worker at a time.
        assert_eq!(world.get::<SubAgentChildren>(e).unwrap().children.len(), 1);
        let first = world.get::<SubAgentChildren>(e).unwrap().children[0];
        // A collect pass while the worker is still running keeps it active and
        // starts nothing new (worker still counts against max_workers).
        fan_out_collect(&mut world);
        assert_eq!(world.get::<SubAgentChildren>(e).unwrap().children.len(), 1);
        assert!(world.get::<FanOutWaiting>(e).is_some());
        complete_worker(&mut world, first, "one");
        fan_out_collect(&mut world);
        // Second worker started after the first finished.
        assert_eq!(world.get::<SubAgentChildren>(e).unwrap().children.len(), 2);
        let second = world.get::<SubAgentChildren>(e).unwrap().children[1];
        complete_worker(&mut world, second, "two");
        fan_out_collect(&mut world);
        assert!(world.get::<FanOutWaiting>(e).is_none());
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    }

    /// A fan-out that leaves `max_workers` out has no cap: every item starts
    /// within the same wake (at most `MAX_WORKER_STARTS_PER_PASS` per pass, so
    /// the tick is never held for the whole queue), and the persisted state
    /// carries no cap, which round-trips through JSON like any other.
    #[test]
    fn collect_with_no_max_workers_starts_every_item_at_once() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 0, WorkerFailure::Continue)),
            r#"[{"id":"a"},{"id":"b"},{"id":"c"},{"id":"d"},{"id":"e"}]"#,
        );
        split(&mut world, e);
        fan_out_collect(&mut world);
        assert_eq!(
            world.get::<SubAgentChildren>(e).unwrap().children.len(),
            MAX_WORKER_STARTS_PER_PASS,
            "one pass starts a bounded number and hands the tick back"
        );
        assert_eq!(world.get::<FanOutWaiting>(e).unwrap().pending.len(), 1);
        fan_out_collect(&mut world);
        assert_eq!(world.get::<SubAgentChildren>(e).unwrap().children.len(), 5);
        let state = world.get::<FanOutWaiting>(e).unwrap().to_state();
        assert_eq!(state.max_workers, None);
        assert!(state.pending.is_empty());
        let json = serde_json::to_string(&state).unwrap();
        let back: FanOutState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_workers, None);
    }

    #[test]
    fn fan_out_state_roundtrips_and_unresolved_workers_become_failures() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailure::Continue)),
            r#"[{"id":"a"},{"id":"b"}]"#,
        );
        split(&mut world, e);
        fan_out_collect(&mut world); // starts both workers → active

        // Projecting to the serializable state captures each worker's run-id.
        let state = world.get::<FanOutWaiting>(e).unwrap().to_state();
        assert_eq!(state.active.len(), 2);
        assert!(state.active.iter().all(|(_id, run_id)| !run_id.is_empty()));

        // Restore onto a fresh parent, resolving run-ids back to entities.
        let by_run: std::collections::HashMap<String, Entity> = world
            .get::<SubAgentChildren>(e)
            .unwrap()
            .children
            .iter()
            .filter_map(|&c| {
                world
                    .get::<crate::persistence::RunMetadata>(c)
                    .map(|m| (m.run_id.clone(), c))
            })
            .collect();
        let fresh = world.spawn_empty().id();
        restore_fan_out_waiting(&mut world, fresh, state.clone(), &|rid| {
            by_run.get(rid).copied()
        });
        assert_eq!(
            world
                .get::<FanOutWaiting>(fresh)
                .unwrap()
                .to_state()
                .active
                .len(),
            2
        );

        // A resolver that can't map the workers → they become failures, so the
        // merge still completes rather than waiting forever.
        let orphaned = world.spawn_empty().id();
        restore_fan_out_waiting(&mut world, orphaned, state, &|_| None);
        let s = world.get::<FanOutWaiting>(orphaned).unwrap().to_state();
        assert!(s.active.is_empty());
        assert_eq!(s.failures.len(), 2);
    }

    #[test]
    fn collect_fail_all_marks_parent_error() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailure::FailAll)),
            r#"[{"id":"a"}]"#,
        );
        split(&mut world, e);
        fan_out_collect(&mut world);
        let worker = world.get::<SubAgentChildren>(e).unwrap().children[0];
        set_status(
            &mut world,
            worker,
            AgentStatus::Error {
                message: "boom".to_string(),
            },
        );
        fan_out_collect(&mut world);
        assert_errored(&world, e);
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 0); // no merge
    }

    #[test]
    fn collect_continue_reports_failures_and_proceeds_without_merge() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        // No merge stage ⇒ ResolveTransition (proceed) rather than force_transition.
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailure::Continue)),
            r#"[{"id":"a"},{"id":"b"}]"#,
        );
        split(&mut world, e);
        fan_out_collect(&mut world);
        let kids = world.get::<SubAgentChildren>(e).unwrap().children.clone();
        set_status(
            &mut world,
            kids[0],
            AgentStatus::Error {
                message: "worker a died".to_string(),
            },
        );
        complete_worker(&mut world, kids[1], "b ok");
        fan_out_collect(&mut world);
        assert!(world.get::<FanOutWaiting>(e).is_none());
        assert!(world.get::<crate::pipeline::ResolveTransition>(e).is_some());
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 0);
    }

    #[test]
    fn collect_finishes_immediately_when_there_are_no_work_items() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailure::Continue)),
            "[]",
        );
        split(&mut world, e);
        fan_out_collect(&mut world);
        // No workers; straight to merge.
        assert!(world.get::<SubAgentChildren>(e).is_none());
        assert!(world.get::<FanOutWaiting>(e).is_none());
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    }

    #[test]
    fn collect_merge_stage_not_found_falls_through_to_transition() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("ghost"), 2, WorkerFailure::Continue)),
            "[]",
        );
        split(&mut world, e);
        fan_out_collect(&mut world);
        // Unknown merge stage ⇒ ResolveTransition, no stage jump.
        assert!(world.get::<crate::pipeline::ResolveTransition>(e).is_some());
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 0);
    }

    #[test]
    fn collect_abandons_a_cancelled_parent() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailure::Continue)),
            r#"[{"id":"a"}]"#,
        );
        split(&mut world, e);
        set_status(&mut world, e, AgentStatus::Cancelled);
        fan_out_collect(&mut world);
        assert!(world.get::<FanOutWaiting>(e).is_none());
        assert_eq!(status_of(&world, e), AgentStatus::Cancelled);
    }

    #[test]
    fn collect_without_a_spawner_records_failures() {
        // No FanOutSpawnerRes installed ⇒ every item fails to start.
        let mut world = World::new();
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailure::Continue)),
            r#"[{"id":"a"}]"#,
        );
        split(&mut world, e);
        fan_out_collect(&mut world);
        // Item failed to start, Continue policy ⇒ still transitions to merge.
        assert!(world.get::<FanOutWaiting>(e).is_none());
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    }

    #[test]
    fn collect_spawner_error_becomes_a_failure() {
        let mut world = World::new();
        install(&mut world, TestSpawner::refusing(&["a"]));
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(None, 2, WorkerFailure::FailAll)),
            r#"[{"id":"a"}]"#,
        );
        split(&mut world, e);
        fan_out_collect(&mut world);
        // Spawn refused + FailAll ⇒ parent errors.
        assert_errored(&world, e);
    }

    // ── start_worker: depth cap + existing SubAgentChildren ───────────────────

    #[test]
    fn start_worker_enforces_depth_cap() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let mut bp = fanout_blueprint(cfg(None, 2, WorkerFailure::Continue));
        bp.max_child_depth = Some(3);
        let e = spawn_parent(&mut world, bp, r#"[{"id":"deep"}]"#);
        // Parent is itself a depth-3 sub-agent ⇒ child would be depth 4 > 3.
        world.entity_mut(e).insert(ParentRef {
            parent_entity: Entity::from_raw_u32(999)
                .expect("a small literal index is always a valid entity id"),
            parent_agent_id: "root".to_string(),
            depth: 3,
        });
        split(&mut world, e);
        fan_out_collect(&mut world);
        // No worker spawned (depth cap hit before any container is created).
        assert!(world.get::<SubAgentChildren>(e).is_none());
        assert!(world.get::<FanOutWaiting>(e).is_none());
    }

    #[test]
    fn start_worker_uses_existing_subagentchildren_cap_and_appends() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            fanout_blueprint(cfg(Some("merge"), 2, WorkerFailure::Continue)),
            r#"[{"id":"a"}]"#,
        );
        // Pre-existing children container with a generous cap.
        world.entity_mut(e).insert(SubAgentChildren {
            children: vec![
                Entity::from_raw_u32(1000)
                    .expect("a small literal index is always a valid entity id"),
            ],
            max_child_depth: 9,
        });
        split(&mut world, e);
        fan_out_collect(&mut world);
        let kids = world.get::<SubAgentChildren>(e).unwrap();
        assert_eq!(kids.max_child_depth, 9);
        assert_eq!(kids.children.len(), 2); // appended to the existing one
    }

    // ── worker_terminal_result / build_report / inject_conversation ───────────

    /// A submitted output beats the last assistant message. A worker whose
    /// final turn is a tool call has no trailing text, so the fallback alone
    /// hands the merge stage an empty string; `submit_output` is the channel a
    /// worker told to report what it did writes into.
    #[test]
    fn a_submitted_answer_beats_the_last_assistant_text() {
        let mut world = World::new();
        let worker = world
            .spawn((
                parent_state(),
                InferenceResult {
                    // What the last-turn fallback alone would hand the merge
                    // stage: the trailing aside, not the deliverable.
                    attempt_id: String::new(),
                    response: "Let me run the tests one more time.".to_string(),
                    tool_calls: vec![],
                    tokens_used: 0,
                    cut_off_at: None,
                    reasoning: None,
                    parts: Vec::new(),
                },
                crate::persistence::FinalOutput(leviath_core::output::FinalOutput::new(
                    "changed src/lib.rs; the failing test now passes",
                    None,
                    "fix_worker".to_string(),
                    0,
                )),
            ))
            .id();
        set_status(&mut world, worker, AgentStatus::Complete);
        assert_eq!(
            worker_terminal_result(&world, worker),
            Some(Ok(
                "changed src/lib.rs; the failing test now passes".to_string()
            ))
        );
    }

    /// The fallback stays, so a blueprint that happens to end on a text turn
    /// keeps working without declaring anything.
    #[test]
    fn a_worker_that_submitted_nothing_still_falls_back_to_its_text() {
        let mut world = World::new();
        let worker = world
            .spawn((
                parent_state(),
                InferenceResult {
                    attempt_id: String::new(),
                    response: "the old behaviour".to_string(),
                    tool_calls: vec![],
                    tokens_used: 0,
                    cut_off_at: None,
                    reasoning: None,
                    parts: Vec::new(),
                },
            ))
            .id();
        set_status(&mut world, worker, AgentStatus::Complete);
        assert_eq!(
            worker_terminal_result(&world, worker),
            Some(Ok("the old behaviour".to_string()))
        );
    }

    /// Spawn a worker sitting in a stage that demands a final output.
    fn spawn_required_output_worker(world: &mut World) -> Entity {
        let stage = StageDef {
            model: model("script", "m"),
            require_output: true,
            ..crate::test_graph::stage("w")
        };
        let layout = layout(
            vec![region("conversation", RegionKind::Clearable, 10_000)],
            12_000,
        );
        let mut bp = crate::test_graph::graph(vec![stage], layout);
        bp.title = Some("w".into());
        let worker = world
            .spawn((parent_state(), both(bp), StageCursor { index: 0 }))
            .id();
        set_status(world, worker, AgentStatus::Complete);
        worker
    }

    /// The fan-out reported "10 succeeded, 0 failed" over ten empty sections,
    /// because a worker that reached `Complete` without its required output was
    /// read as a success with nothing to say. The merge stage cannot tell those
    /// apart, so it writes a confident merge of nothing.
    ///
    /// This is the ordinary way it happens, not an edge case: a worker that
    /// cannot satisfy its validator retries until its iterations run out and
    /// leaves on the max-iterations path, which ends at `Complete`.
    #[test]
    fn a_worker_that_owes_an_output_and_has_none_is_a_failure() {
        let mut world = World::new();
        let worker = spawn_required_output_worker(&mut world);

        assert_eq!(
            worker_terminal_result(&world, worker),
            Some(Err(
                "worker finished without the final output its stage requires".to_string()
            )),
            "the merge has to be told a worker failed, and why"
        );
    }

    /// The same worker, having actually submitted: its answer is what it
    /// contributes, and the requirement is discharged.
    #[test]
    fn a_worker_that_owes_an_output_and_has_one_contributes_it() {
        let mut world = World::new();
        let worker = spawn_required_output_worker(&mut world);
        world
            .entity_mut(worker)
            .insert(crate::persistence::FinalOutput(
                leviath_core::output::FinalOutput {
                    content: "the rows".to_string(),
                    format: Some("csv".to_string()),
                    stage: "w".to_string(),
                    submitted_at: 0,
                    truncated: false,
                    artifacts: vec![],
                },
            ));

        assert_eq!(
            worker_terminal_result(&world, worker),
            Some(Ok("the rows".to_string()))
        );
    }

    /// A worker with a blueprint but no cursor cannot be placed in a stage, so
    /// there is no stage to read a requirement off. It keeps the fallback rather
    /// than being called a failure for a question that was never asked.
    #[test]
    fn a_worker_with_no_stage_to_read_owes_nothing() {
        let mut world = World::new();
        let bp = fanout_blueprint(cfg(None, 1, WorkerFailure::Continue));

        // No blueprint at all.
        let bare = world.spawn(parent_state()).id();
        assert!(!worker_requires_output(&world, bare));

        // A blueprint, but no cursor saying which stage it is in.
        let no_cursor = world.spawn((parent_state(), both(bp))).id();
        assert!(!worker_requires_output(&world, no_cursor));

        // A cursor pointing past the end of the stage list.
        let past_end = world
            .spawn((
                parent_state(),
                both(fanout_blueprint(cfg(None, 1, WorkerFailure::Continue))),
                StageCursor { index: 99 },
            ))
            .id();
        assert!(!worker_requires_output(&world, past_end));
    }

    /// A blueprint that never opted in keeps the last-turn fallback, empty text
    /// and all. Turning that into a failure would break every fan-out that does
    /// not declare `require_output`.
    #[test]
    fn a_worker_that_owes_nothing_keeps_the_last_turn_fallback() {
        let mut world = World::new();
        let worker = world.spawn(parent_state()).id();
        set_status(&mut world, worker, AgentStatus::Complete);

        assert_eq!(
            worker_terminal_result(&world, worker),
            Some(Ok(String::new()))
        );
    }

    #[test]
    fn worker_terminal_result_covers_every_status() {
        let mut world = World::new();
        let complete = world
            .spawn((
                parent_state(),
                InferenceResult {
                    attempt_id: String::new(),
                    response: "done text".to_string(),
                    tool_calls: vec![],
                    tokens_used: 0,
                    cut_off_at: None,
                    reasoning: None,
                    parts: Vec::new(),
                },
            ))
            .id();
        set_status(&mut world, complete, AgentStatus::Complete);
        assert_eq!(
            worker_terminal_result(&world, complete),
            Some(Ok("done text".to_string()))
        );

        let complete_no_infer = world.spawn(parent_state()).id();
        set_status(&mut world, complete_no_infer, AgentStatus::Complete);
        assert_eq!(
            worker_terminal_result(&world, complete_no_infer),
            Some(Ok(String::new()))
        );

        let errored = world.spawn(parent_state()).id();
        set_status(
            &mut world,
            errored,
            AgentStatus::Error {
                message: "x".to_string(),
            },
        );
        assert_eq!(
            worker_terminal_result(&world, errored),
            Some(Err("x".to_string()))
        );

        let cancelled = world.spawn(parent_state()).id();
        set_status(&mut world, cancelled, AgentStatus::Cancelled);
        assert!(worker_terminal_result(&world, cancelled).is_some_and(|r| r.is_err()));

        let running = world.spawn(parent_state()).id(); // Active
        assert_eq!(worker_terminal_result(&world, running), None);

        assert!(
            worker_terminal_result(
                &world,
                Entity::from_raw_u32(4242)
                    .expect("a small literal index is always a valid entity id")
            )
            .is_some_and(|r| r.is_err())
        );
    }

    /// The failure this bound exists for. A hundred workers answering at the
    /// size limit build a 25 MB report; `add_entry` rejects an over-budget entry
    /// rather than truncating, and the error was discarded - so the merge stage
    /// received nothing at all, silently, in exactly the case fan-out is for.
    #[test]
    fn a_huge_fan_out_still_reaches_the_merge_stage() {
        let mut world = World::new();
        let mut window = ContextWindow::new(100_000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::Clearable,
            10_000,
        ));
        let parent = world.spawn((parent_state(), window)).id();

        // A hundred workers, each answering at the per-submission cap.
        let huge = "x".repeat(leviath_core::output::MAX_FINAL_OUTPUT_BYTES);
        let summaries: Vec<(String, String)> =
            (0..100).map(|i| (format!("w{i}"), huge.clone())).collect();
        let report = build_report(&summaries, &[], Some(10_000));
        inject_results(&mut world, parent, "conversation", &report, Vec::new());

        let region = world
            .get::<ContextWindow>(parent)
            .expect("window")
            .get_region("conversation")
            .expect("region");
        assert!(
            !region.content.is_empty(),
            "the merge stage must receive something rather than nothing"
        );
        let landed = &region.content[0].content;
        // Every worker is still accounted for in the header, and the text says
        // it was cut rather than pretending to be whole.
        assert!(landed.contains("100 succeeded"), "header survives");
        assert!(landed.contains("truncated"), "and says it was cut");
        assert!(region.current_tokens <= region.max_tokens, "within budget");
    }

    /// Dividing the region between the workers makes the report fit an *empty*
    /// region, which is the easy case. A region already carrying something has
    /// less room than that, and the report-level trim is what keeps the write
    /// from being rejected outright: `add_entry` refuses an over-budget entry
    /// rather than shortening it, so without this the merge stage receives
    /// nothing at all.
    #[test]
    fn a_report_larger_than_what_is_left_of_the_region_is_trimmed_not_dropped() {
        const REGION_TOKENS: usize = 2_000;
        let mut world = World::new();
        let mut window = ContextWindow::new(100_000);
        window.add_region(leviath_core::Region::new(
            "worker_results".to_string(),
            leviath_core::RegionKind::Clearable,
            REGION_TOKENS,
        ));
        // Most of the region is already spoken for.
        let filler = "f".repeat(REGION_TOKENS * 4 * 8 / 10);
        let filler_tokens = leviath_core::estimate_tokens(&filler);
        window
            .add_typed_entry(
                "worker_results",
                leviath_core::EntryKind::UserMessage,
                filler,
                filler_tokens,
            )
            .expect("the filler fits");
        let parent = world.spawn((parent_state(), window)).id();

        // A report sized for the whole region, landing in what is left of it.
        let long = "x".repeat(5_000);
        let summaries: Vec<(String, String)> =
            (0..8).map(|i| (format!("w{i}"), long.clone())).collect();
        let report = build_report(&summaries, &[], Some(REGION_TOKENS));
        assert!(report.len() > REGION_TOKENS * 4 / 5, "the report is big");
        inject_results(&mut world, parent, "worker_results", &report, Vec::new());

        let region = world
            .get::<ContextWindow>(parent)
            .expect("window")
            .get_region("worker_results")
            .expect("region")
            .clone();
        assert_eq!(
            region.content.len(),
            2,
            "the report landed beside the filler"
        );
        let landed = &region.content[1].content;
        assert!(
            landed.contains("8 succeeded"),
            "the header survives the cut"
        );
        assert!(
            landed.contains(REPORT_TRUNCATION_MARKER.trim()),
            "and it says it was cut"
        );
        assert!(region.current_tokens <= region.max_tokens, "within budget");
    }

    /// The share is equal, so every worker appears. The first cut capped each
    /// worker at a fixed size and trimmed the finished report to fit, which gave
    /// the early workers their full allowance and cut the late ones off
    /// entirely - a hundred-way fan-out where only the first twenty were
    /// readable, with nothing saying so.
    #[test]
    fn every_worker_appears_in_a_large_fan_out() {
        // End to end: building the report and landing it in the region. The
        // unfairness was in the second half - a fixed per-worker size makes a
        // report far too big, and trimming *that* keeps the front and drops the
        // back.
        const REGION_TOKENS: usize = 40_000;
        let mut world = World::new();
        let mut window = ContextWindow::new(400_000);
        window.add_region(leviath_core::Region::new(
            "worker_results".to_string(),
            leviath_core::RegionKind::Clearable,
            REGION_TOKENS,
        ));
        let parent = world.spawn((parent_state(), window)).id();

        let long = "x".repeat(50_000);
        let summaries: Vec<(String, String)> =
            (0..100).map(|i| (format!("w{i}"), long.clone())).collect();
        let report = build_report(&summaries, &[], Some(REGION_TOKENS));
        inject_results(&mut world, parent, "worker_results", &report, Vec::new());

        let landed = world
            .get::<ContextWindow>(parent)
            .expect("window")
            .get_region("worker_results")
            .expect("region")
            .content[0]
            .content
            .clone();
        for i in 0..100 {
            assert!(
                landed.contains(&format!("## worker w{i}\n")),
                "worker w{i} never reached the merge stage"
            );
        }
        // And it says the sections are extracts, so the merge stage knows to go
        // to a worker's own run for the rest.
        assert!(landed.contains("read a worker's own run"));
    }

    /// Each worker gets the same room, whatever the count.
    #[test]
    fn the_share_shrinks_as_the_worker_count_grows() {
        assert!(bytes_per_worker(Some(40_000), 4) > bytes_per_worker(Some(40_000), 100));
        // A bigger region means a bigger share for the same workers.
        assert!(bytes_per_worker(Some(80_000), 10) > bytes_per_worker(Some(40_000), 10));
        // Never so small a section says nothing at all.
        assert_eq!(
            bytes_per_worker(Some(10), 10_000),
            MIN_REPORT_BYTES_PER_WORKER
        );
        // No readable budget falls back rather than dividing by nothing.
        assert_eq!(bytes_per_worker(None, 4), DEFAULT_REPORT_BYTES_PER_WORKER);
    }

    /// A blueprint can send the results somewhere other than the conversation,
    /// which is otherwise carrying the message history alongside them.
    #[test]
    fn results_go_to_the_named_region() {
        let mut world = World::new();
        let mut window = ContextWindow::new(100_000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::Clearable,
            10_000,
        ));
        window.add_region(leviath_core::Region::new(
            "worker_results".to_string(),
            leviath_core::RegionKind::Clearable,
            20_000,
        ));
        let parent = world.spawn((parent_state(), window)).id();
        inject_results(
            &mut world,
            parent,
            "worker_results",
            "the report",
            Vec::new(),
        );

        let w = world.get::<ContextWindow>(parent).expect("window");
        assert_eq!(
            w.get_region("worker_results")
                .expect("region")
                .content
                .len(),
            1
        );
        assert!(
            w.get_region("conversation")
                .expect("region")
                .content
                .is_empty(),
            "the default region is left alone"
        );
    }

    /// A named region the layout does not declare falls back rather than
    /// swallowing the whole report.
    #[test]
    fn an_unknown_results_region_falls_back_to_the_conversation() {
        let mut world = World::new();
        let mut window = ContextWindow::new(100_000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::Clearable,
            10_000,
        ));
        let parent = world.spawn((parent_state(), window)).id();
        inject_results(&mut world, parent, "typo_region", "the report", Vec::new());

        assert_eq!(
            world
                .get::<ContextWindow>(parent)
                .expect("window")
                .get_region("conversation")
                .expect("region")
                .content
                .len(),
            1
        );
    }

    /// A report that fits is passed through untouched, so the common case reads
    /// exactly as it did.
    #[test]
    fn a_small_fan_out_report_is_not_trimmed() {
        let mut world = World::new();
        let mut window = ContextWindow::new(100_000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::Clearable,
            10_000,
        ));
        let parent = world.spawn((parent_state(), window)).id();
        let report = build_report(
            &[("a".to_string(), "did the thing".to_string())],
            &[],
            Some(10_000),
        );
        inject_results(&mut world, parent, "conversation", &report, Vec::new());
        let landed = world
            .get::<ContextWindow>(parent)
            .expect("window")
            .get_region("conversation")
            .expect("region")
            .content[0]
            .content
            .clone();
        assert_eq!(landed, report);
    }

    /// A run at its ceiling stops widening.
    ///
    /// Refused at the spawn rather than at the split, so the workers already
    /// running keep going and the merge still happens on what came back. A run
    /// that stopped widening is a cheaper answer, not a failure.
    #[test]
    fn a_run_at_its_ceiling_does_not_spawn_another_worker() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let parent = world
            .spawn((parent_state(), spec_c("t", fanout_graph())))
            .id();

        let item = item("one");
        let config = cfg(None, 3, WorkerFailure::Continue);

        // No ceiling: the spawn goes through, and the run now holds two agents.
        world.insert_resource(FanOutBudget(0));
        assert!(start_worker(&mut world, parent, &config, &item).is_ok());
        assert_eq!(run_tree_size(&world, parent), 2);

        // A ceiling of two, already reached.
        world.insert_resource(FanOutBudget(2));
        let refused = start_worker(&mut world, parent, &config, &item)
            .expect_err("the run is at its ceiling");
        assert!(refused.contains("ceiling is 2"), "{refused}");
        assert!(
            refused.contains("max_agents_per_run"),
            "names the knob that set it: {refused}"
        );
        assert_eq!(run_tree_size(&world, parent), 2, "and nothing was spawned");

        // Raised: it widens again, so this is a ceiling and not a latch.
        world.insert_resource(FanOutBudget(3));
        assert!(start_worker(&mut world, parent, &config, &item).is_ok());
        assert_eq!(run_tree_size(&world, parent), 3);
    }

    /// The headcount is the run's, not the branch's.
    ///
    /// A depth-2 worker asking "how many of us are there" must not answer with
    /// the size of its own subtree, or every branch gets the whole budget and
    /// the ceiling multiplies by however many branches there are.
    #[test]
    fn the_run_headcount_is_counted_from_the_root() {
        let mut world = World::new();
        let root = world.spawn_empty().id();
        let a = world.spawn_empty().id();
        let b = world.spawn_empty().id();
        let leaf = world.spawn_empty().id();

        world.entity_mut(root).insert(SubAgentChildren {
            children: vec![a, b],
            max_child_depth: 2,
        });
        for (child, depth) in [(a, 1), (b, 1)] {
            world.entity_mut(child).insert(ParentRef {
                parent_entity: root,
                parent_agent_id: "root".to_string(),
                depth,
            });
        }
        world.entity_mut(a).insert(SubAgentChildren {
            children: vec![leaf],
            max_child_depth: 2,
        });
        world.entity_mut(leaf).insert(ParentRef {
            parent_entity: a,
            parent_agent_id: "a".to_string(),
            depth: 2,
        });

        // Four agents: the root, two workers, and one grandchild.
        assert_eq!(run_tree_size(&world, root), 4, "counted from the root");
        assert_eq!(
            run_tree_size(&world, leaf),
            4,
            "and the same from the deepest leaf, which is the point"
        );
        assert_eq!(run_tree_size(&world, b), 4, "and from a childless branch");
    }

    /// A lone agent is one agent, so a run with no sub-agents is not somehow
    /// zero and does not get a free spawn past a ceiling of one.
    #[test]
    fn a_run_with_no_sub_agents_counts_itself() {
        let mut world = World::new();
        let solo = world.spawn_empty().id();
        assert_eq!(run_tree_size(&world, solo), 1);
    }

    /// A blueprint's fan-out ceiling reaches a split made through the tool.
    ///
    /// `max_items` lives on a `mode = "fan_out"` stage, and the tool is called
    /// from ordinary stages, so a tool-driven split saw no ceiling and made as
    /// many workers as the model named. Measured on a blueprint declaring
    /// `max_items = 3`: splits through this door made five and six, and one run
    /// reached 34 sub-agents where an earlier one reached 7.
    #[test]
    fn a_tool_split_takes_the_blueprints_declared_ceiling() {
        let bp = fanout_blueprint(FanOutDef {
            max_items: Some(3),
            ..cfg(None, 3, WorkerFailure::Continue)
        });
        let mut world = World::new();
        // Cursor on the merge stage rather than the fan-out one, because that is
        // the situation: the tool is called from a stage that declares nothing.
        let e = world.spawn((both(bp), StageCursor { index: 1 })).id();

        assert_eq!(
            graph_fan_out_max_items(&world, e),
            Some(3),
            "the ceiling the blueprint wrote, found from a stage that does not \
             declare it"
        );
    }

    /// An author who wrote no ceiling is not given one. The fix carries a
    /// declared number to a second door; it does not invent a number.
    #[test]
    fn a_blueprint_declaring_no_ceiling_still_has_none() {
        let bp = fanout_blueprint(cfg(None, 3, WorkerFailure::Continue));
        let mut world = World::new();
        let e = world.spawn((both(bp), StageCursor { index: 1 })).id();
        assert_eq!(graph_fan_out_max_items(&world, e), None);

        // An entity carrying no blueprint declares nothing either, rather than
        // the lookup being an error.
        let bare = world.spawn_empty().id();
        assert_eq!(graph_fan_out_max_items(&world, bare), None);
    }

    #[test]
    fn build_report_lists_successes_and_failures() {
        let report = build_report(
            &[("a".to_string(), "ok-a".to_string())],
            &[("b".to_string(), "boom".to_string())],
            None,
        );
        assert!(report.contains("1 succeeded, 1 failed"));
        assert!(report.contains("## worker a\nok-a"));
        assert!(report.contains("## worker b FAILED\nboom"));
    }

    #[test]
    fn inject_conversation_is_a_noop_without_a_window() {
        let mut world = World::new();
        let has_window = world.spawn(window()).id();
        inject_results(&mut world, has_window, "conversation", "hello", Vec::new());
        assert!(
            world
                .get::<ContextWindow>(has_window)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .current_tokens
                > 0
        );
        // Entity without a ContextWindow: silently ignored.
        let no_window = world.spawn(parent_state()).id();
        inject_results(&mut world, no_window, "conversation", "hello", Vec::new());
    }

    /// The run metadata a worker or parent carries, enough for the store to
    /// key its files by.
    pub(super) fn run_meta(run_id: &str) -> crate::persistence::RunMetadata {
        crate::persistence::RunMetadata {
            run_id: run_id.to_string(),
            agent_name: "a".to_string(),
            agent_path: String::new(),
            task: String::new(),
            model: None,
            workdir: String::new(),
            num_stages: 1,
            started_at: 0,
            parent_run_id: None,
            metadata: std::collections::HashMap::new(),
            callback_url: None,
            callback_secret: None,
            title: None,
            title_error: None,
            blueprint_digest: None,
            unattended: false,
            yolo_profile: None,
            read_paths: None,
            output_request: None,
            model_override: None,
        }
    }

    /// A worker's files travel up with its text: re-stored under the parent,
    /// named after the worker, typed as the worker declared them. A file the
    /// store lost, one never stored (no hash) and one over the ceiling are
    /// left out; a worker with no output, or a world with no store or
    /// registry, hands up nothing.
    #[test]
    fn a_finished_workers_artifacts_are_handed_up_as_parts() {
        fn hand_up_artifacts(world: &World, parent: Entity, aw: &ActiveWorker) -> Vec<Part> {
            io::HandUp::of(world, parent, aw)
                .map(|job| job.run().parts)
                .unwrap_or_default()
        }
        let mut world = World::new();
        let store: Arc<dyn leviath_core::mime::BlobStore> =
            Arc::new(leviath_core::mime::MemoryBlobStore::new());
        let registry = leviath_core::mime::MimeRegistry::builtin();
        let png = leviath_core::mime::MimeType::parse("image/png").unwrap();
        let hero = store
            .put(
                "run-w",
                &leviath_core::mime::Blob {
                    mime_type: png.clone(),
                    bytes: b"\x89PNG fake".to_vec(),
                    name: Some("hero.png".to_string()),
                },
                &registry,
            )
            .unwrap();
        world.insert_resource(BlobStoreHandle(store.clone()));
        world.insert_resource(MimeRegistryHandle(Arc::new(registry)));
        let parent = world.spawn((parent_state(), run_meta("run-p"))).id();
        let artifact = |name: &str, sha: String| Artifact {
            name: name.to_string(),
            path: format!("{name}.png"),
            mime_type: png.clone(),
            size: 9,
            sha256: sha,
        };
        let worker = world
            .spawn((
                parent_state(),
                run_meta("run-w"),
                crate::persistence::FinalOutput(
                    leviath_core::output::FinalOutput::new("drawn", None, "s".to_string(), 0)
                        .with_artifacts(vec![
                            artifact("hero", hero.sha256.clone()),
                            artifact("lost", "00".repeat(32)),
                            Artifact::from_path("untracked.txt"),
                        ]),
                ),
            ))
            .id();
        let aw = ActiveWorker {
            item_id: "w1".to_string(),
            entity: worker,
            run_id: "run-w".to_string(),
        };

        let parts = hand_up_artifacts(&world, parent, &aw);
        assert_eq!(
            parts.len(),
            1,
            "the lost and the unstored files are left out"
        );
        assert_eq!(parts[0].name.as_deref(), Some("w1/hero"));
        let blob = parts[0].blob().expect("a stored part");
        assert_eq!(blob.sha256, hero.sha256);
        assert_eq!(blob.mime_type, png);
        assert!(
            store.read("run-p", &hero.sha256).is_ok(),
            "the bytes now sit under the parent's run"
        );

        // Over the part ceiling: skipped with a warning, not an error.
        world.insert_resource(MimeLimits {
            max_part_bytes: 1,
            ..MimeLimits::default()
        });
        assert!(hand_up_artifacts(&world, parent, &aw).is_empty());

        // Only unstored files: nothing to hand up before any store is asked.
        let bare = world
            .spawn((
                parent_state(),
                crate::persistence::FinalOutput(
                    leviath_core::output::FinalOutput::new("said", None, "s".to_string(), 0)
                        .with_artifacts(vec![Artifact::from_path("untracked.txt")]),
                ),
            ))
            .id();
        let bare = ActiveWorker {
            item_id: "w2".to_string(),
            entity: bare,
            run_id: "run-b".to_string(),
        };
        assert!(hand_up_artifacts(&world, parent, &bare).is_empty());

        // No output at all.
        let silent = world.spawn(parent_state()).id();
        let silent = ActiveWorker {
            item_id: "w3".to_string(),
            entity: silent,
            run_id: "run-s".to_string(),
        };
        assert!(hand_up_artifacts(&world, parent, &silent).is_empty());

        // A world with no registry, then none with a store either.
        world.remove_resource::<MimeLimits>();
        world.remove_resource::<MimeRegistryHandle>();
        assert!(hand_up_artifacts(&world, parent, &aw).is_empty());
        world.remove_resource::<BlobStoreHandle>();
        assert!(hand_up_artifacts(&world, parent, &aw).is_empty());
    }

    /// The parts ride on the report's own entry, charged their stand-ins.
    #[test]
    fn injected_results_carry_the_workers_parts() {
        let mut world = World::new();
        let parent = world.spawn((parent_state(), window())).id();
        let part = Part::stored(leviath_core::mime::BlobRef {
            sha256: "ab".repeat(32),
            mime_type: leviath_core::mime::MimeType::parse("image/png").unwrap(),
            size: 9,
            width: None,
            height: None,
            duration_ms: None,
            tokens: 400,
            stand_in: "[image/png 9 B] w1/hero".to_string(),
        })
        .named("w1/hero");
        inject_results(&mut world, parent, "conversation", "the report", vec![part]);
        let window = world.get::<ContextWindow>(parent).unwrap();
        let region = window.get_region("conversation").unwrap();
        assert_eq!(region.content.len(), 1);
        let entry = &region.content[0];
        assert_eq!(entry.content.parts()[0].inline_text(), Some("the report"));
        assert!(entry.content.has_stored());
        assert_eq!(entry.content.parts()[1].name.as_deref(), Some("w1/hero"));
        assert!(
            entry.tokens < 400,
            "charged the stand-in, not the native estimate: {}",
            entry.tokens
        );
    }

    #[test]
    fn set_status_is_a_noop_for_a_missing_agent() {
        let mut world = World::new();
        set_status(
            &mut world,
            Entity::from_raw_u32(77).expect("a small literal index is always a valid entity id"),
            AgentStatus::Complete,
        );
        assert_eq!(
            agent_status(
                &world,
                Entity::from_raw_u32(77)
                    .expect("a small literal index is always a valid entity id")
            ),
            None
        );
    }

    // ── force_transition (pipeline helper) edge cases via fan-out ─────────────

    #[test]
    fn force_transition_applies_routing_and_handles_despawn_and_overflow() {
        use crate::pipeline::force_transition;
        // Routing present on the target stage ⇒ ToolResultRoutingComponent added.
        let mut world = World::new();
        let routing = crate::spec::graph::ToolRoutingDef {
            default_region: RegionName::new("conversation").unwrap(),
            tool_regions: Default::default(),
            keep_results: false,
            max_result_tokens: None,
            tool_max_result_tokens: Default::default(),
        };
        let mut bp = fanout_blueprint(cfg(Some("merge"), 2, WorkerFailure::Continue));
        bp.stages[1].tool_routing = Some(routing);
        let e = world
            .spawn((
                both(bp),
                StageCursor { index: 0 },
                parent_state(),
                StageProgress::default(),
                VisitCounts::default(),
                window(),
            ))
            .id();
        let agent = crate::world::AgentId::in_world(&world, e);
        force_transition(&mut world, agent, 1);
        assert!(world.get::<ToolResultRoutingComponent>(e).is_some());
        assert!(world.get::<ReadyToInfer>(e).is_some());

        // Despawned entity: no panic, no effect.
        let gone = crate::world::AgentId::in_world(
            &world,
            Entity::from_raw_u32(9191).expect("a small literal index is always a valid entity id"),
        );
        force_transition(&mut world, gone, 1);
    }

    #[test]
    fn force_transition_marks_error_on_prompt_overflow() {
        use crate::pipeline::force_transition;
        // A tiny pinned region + a huge stage system prompt ⇒ overflow on entry.
        let layout = layout(vec![region("task", RegionKind::Pinned, 20)], 1000);
        let s0 = StageDef {
            model: model("script", "m"),
            mode: Mode::FanOut(cfg(Some("merge"), 2, WorkerFailure::Continue)),
            ..crate::test_graph::stage("fan")
        };
        let s1 = StageDef {
            model: model("script", "m"),
            system_prompt: Some("x".repeat(10_000)),
            ..crate::test_graph::stage("merge")
        };
        let bp = crate::test_graph::graph(vec![s0, s1], layout);

        let mut w = ContextWindow::new(1000);
        w.add_region(Region::new("task".to_string(), RegionKind::Pinned, 20));
        let (mut world, e) = world_with(bp, w);
        let agent = crate::world::AgentId::in_world(&world, e);
        force_transition(&mut world, agent, 1);
        assert_errored(&world, e);
    }

    /// Build a world with one agent carrying the given blueprint and window.
    fn world_with(bp: RunGraph, w: ContextWindow) -> (World, Entity) {
        let mut world = World::new();
        let e = world
            .spawn((
                both(bp),
                StageCursor { index: 0 },
                parent_state(),
                StageProgress::default(),
                VisitCounts::default(),
                w,
            ))
            .id();
        (world, e)
    }

    // ── typed work items ──────────────────────────────────────────────────────

    /// A fan-out stage whose graph declares a `topic` text input.
    pub(super) fn topic_blueprint(config: FanOutDef) -> RunGraph {
        use crate::spec::inputs::{InputDecl, InputSlot, InputType, RegionBinding};
        let mut bp = fanout_blueprint(config);
        bp.layout
            .regions
            .push(region("topic", RegionKind::Pinned, 1000));
        bp.inputs.push(InputDecl {
            name: crate::spec::names::InputName::new("topic").unwrap(),
            ty: InputType::Text {
                multiline: true,
                min_len: None,
                max_len: None,
            },
            required: false,
            default: None,
            description: None,
            binds: vec![InputSlot::Region(RegionBinding {
                region: RegionName::new("topic").unwrap(),
                template: None,
            })],
        });
        bp
    }

    pub(super) fn pending(world: &mut World, e: Entity, args: serde_json::Value) {
        world.entity_mut(e).insert(PendingFanOut {
            call_id: "call-1".to_string(),
            request: parse_fan_out_call(&args).unwrap(),
        });
    }

    /// A same-graph worker's items are checked against the graph's inputs
    /// before any worker starts. A mistyped one is refused at its path, and
    /// the model is handed the refusal to correct.
    #[test]
    fn a_mistyped_work_item_is_refused_before_any_worker_starts() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let e = spawn_parent(
            &mut world,
            topic_blueprint(cfg(Some("merge"), 2, WorkerFailure::Continue)),
            "",
        );
        pending(
            &mut world,
            e,
            serde_json::json!({"items": [
                {"id": "a", "inputs": {"topic": "rust"}},
                {"id": "b", "inputs": {"topic": 4}}
            ]}),
        );

        start_pending_fan_outs(&mut world);

        assert!(world.get::<FanOutWaiting>(e).is_none(), "nothing started");
        assert!(world.get::<crate::pipeline::ReadyToInfer>(e).is_some());
        let convo = conversation_text(&world, e);
        assert!(
            convo.contains("[error] fan_out items do not fit"),
            "{convo}"
        );
        assert!(convo.contains("items[1].inputs.topic"), "{convo}");

        // Typed right, the same items start.
        pending(
            &mut world,
            e,
            serde_json::json!({"items": [{"id": "a", "inputs": {"topic": "rust"}}]}),
        );
        start_pending_fan_outs(&mut world);
        assert_eq!(
            world.get::<FanOutWaiting>(e).expect("parked").pending.len(),
            1
        );
    }

    /// A spawner that reads every worker blueprint as taking only `topic`
    /// text, and starts workers as [`TestSpawner`] does.
    struct Typed(Arc<dyn FanOutSpawner>);

    impl FanOutSpawner for Typed {
        fn prepare_worker(&self, request: SpawnRequest, caller: Caller) -> WorkerPrep {
            self.0.prepare_worker(request, caller)
        }

        fn find_worker(&self, query: &str) -> Result<BlueprintRef, String> {
            self.0.find_worker(query)
        }

        fn worker_inputs(&self, _source: &SpawnSource) -> Option<Vec<InputDecl>> {
            Some(topic_blueprint(cfg(None, 1, WorkerFailure::Continue)).inputs)
        }
    }

    /// A worker blueprint's items are checked against the inputs it
    /// declares, before any worker starts, each at its item's path.
    #[test]
    fn a_blueprint_workers_items_are_checked_against_its_inputs() {
        let mut world = World::new();
        install(&mut world, Arc::new(Typed(TestSpawner::ok())));
        let mut bp = fanout_blueprint(cfg(None, 2, WorkerFailure::Continue));
        bp.stages[0].mode = Mode::Autonomous;
        let e = spawn_parent(&mut world, bp, "");
        let call = |topic: serde_json::Value| serde_json::json!({"agent": "probe", "items": [{"id": "a", "inputs": {"topic": topic}}]});
        pending(&mut world, e, call(serde_json::json!(4)));
        start_pending_fan_outs(&mut world);
        assert!(world.get::<FanOutWaiting>(e).is_none(), "nothing started");
        let convo = conversation_text(&world, e);
        assert!(convo.contains("items[0].inputs.topic"), "{convo}");

        pending(&mut world, e, call(serde_json::json!("rust")));
        start_pending_fan_outs(&mut world);
        assert!(
            world.get::<FanOutWaiting>(e).is_some(),
            "a typed item starts"
        );
        fan_out_collect(&mut world);
        assert_eq!(
            world.get::<FanOutWaiting>(e).map(|w| w.active.len()),
            Some(1),
            "its worker was started"
        );
    }

    /// Which inputs a named, pathed or queried worker blueprint is held to
    /// before it starts: the spawner's reading of it, and none when nothing
    /// can say. A world with no runtime reads them in place.
    #[test]
    fn a_workers_inputs_come_from_the_spawner() {
        use io::WorkerBlueprint as B;
        let blueprint = || {
            B::Named(SpawnSource::Blueprint(
                BlueprintRef::parse("probe").unwrap(),
            ))
        };
        let file = || {
            B::Named(SpawnSource::BlueprintFile(
                crate::spec::names::BlueprintPath::new(
                    std::env::temp_dir().join("probe").to_string_lossy(),
                )
                .unwrap(),
            ))
        };
        let e = Entity::PLACEHOLDER;
        let mut world = World::new();
        let read = |world: &World, b| io::read_inputs(world, e, b).expect("read in place");
        assert!(read(&world, blueprint()).is_none(), "no spawner");
        install(&mut world, Arc::new(Typed(TestSpawner::ok())));
        for worker in [blueprint(), file(), B::Query("tests".into())] {
            assert!(read(&world, worker).is_some());
        }
        assert!(read(&world, B::Query("nobody".into())).is_none());
        install(&mut world, TestSpawner::ok());
        assert!(read(&world, blueprint()).is_none(), "unread");
    }

    /// An ordinary stage has no worker to fall back on, so a call that names
    /// none is refused rather than failing every item.
    #[test]
    fn a_call_naming_no_worker_outside_a_fan_out_stage_is_refused() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let mut bp = fanout_blueprint(cfg(None, 2, WorkerFailure::Continue));
        bp.stages[0].mode = Mode::Autonomous;
        let e = spawn_parent(&mut world, bp, "");
        pending(&mut world, e, serde_json::json!({"items": [{"id": "a"}]}));

        start_pending_fan_outs(&mut world);

        assert!(world.get::<FanOutWaiting>(e).is_none());
        let convo = conversation_text(&world, e);
        assert!(convo.contains("needs an `agent`"), "{convo}");
    }

    /// Each worker source reaches the spawner as the request it means: an
    /// installed blueprint by name, one a query found, or the parent's own
    /// graph. A query nothing answers fails that item.
    #[test]
    fn every_worker_source_becomes_a_spawn_request() {
        let mut world = World::new();
        install(&mut world, TestSpawner::ok());
        let parent = world
            .spawn((parent_state(), spec_c("t", fanout_graph())))
            .id();
        let mut config = cfg(None, 3, WorkerFailure::Continue);
        config.worker = WorkerSource::Blueprint(BlueprintRef::parse("fixer").unwrap());
        assert!(start_worker(&mut world, parent, &config, &item("a")).is_ok());
        config.worker = WorkerSource::Query("tests".into());
        assert!(start_worker(&mut world, parent, &config, &item("b")).is_ok());
        let dir = std::env::temp_dir().join("fixer");
        config.worker = WorkerSource::BlueprintFile(
            crate::spec::names::BlueprintPath::new(dir.to_string_lossy()).unwrap(),
        );
        assert!(start_worker(&mut world, parent, &config, &item("e")).is_ok());
        config.worker = WorkerSource::Query("nobody".into());
        let err = start_worker(&mut world, parent, &config, &item("c")).unwrap_err();
        assert!(err.contains("no installed agent"), "{err}");

        // A parent with no spec has nothing to start a worker from.
        let bare = world.spawn(parent_state()).id();
        let err = start_worker(&mut world, bare, &config, &item("d")).unwrap_err();
        assert!(err.contains("no run spec"), "{err}");
    }
}

#[cfg(test)]
#[path = "fanout/starts_tests.rs"]
mod starts_tests;

#[cfg(test)]
#[path = "fanout/io_tests.rs"]
mod io_tests;

#[cfg(test)]
#[path = "fanout/adopt_tests.rs"]
mod adopt_tests;
