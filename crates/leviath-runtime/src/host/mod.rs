//! The world host: the daemon-side wrapper that owns a single [`PipelineWorld`],
//! maps stable **run ids** to ECS entities, and interleaves external **control
//! operations** with driving the world - all on one task, so there is never any
//! locking around the world.
//!
//! Clients (a control socket, the TUI, the CLI) don't hold entities - those are
//! generational indices meaningful only inside the world. They address agents by
//! run id. The host keeps the `run_id → Entity` map and turns each
//! [`ControlOp`] into the corresponding [`PipelineWorld`] call, replying on the
//! op's oneshot channel.
//!
//! The serve loop drives the world to quiescence, then parks until either an
//! async result wakes it, a control op arrives, or shutdown is signalled -
//! handling a control op and then re-driving to quiescence so its effect (a
//! resume, a delivered message) is applied immediately.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use bevy_ecs::entity::Entity;
use tokio::sync::broadcast;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::components::{
    AgentMessage, AgentState, AgentStatus, AwaitingInteraction, ContextWindow, ParentRef,
    SubAgentChildren, WaitReason,
};
use crate::interaction_hub::{AnswerError, InteractionHub};
use crate::persistence::{RunMetadata, TokenTotals};
use crate::world::{AgentId, LaneSnapshot, PipelineWorld};

// Sections of the former single-file host, one per concern. Glob re-exported so
// every existing `host::ControlOp` / `host::WorldEvent` path keeps working and
// the split stays a pure move.
mod events;
pub use events::*;
mod types;
pub use types::*;
mod settings;
pub use settings::HostSettings;

/// Owns the world and the run-id map; drives the world and services control ops.
pub struct WorldHost {
    world: PipelineWorld,
    by_run_id: HashMap<String, AgentId>,
    interactions: InteractionHub,
    starter: Option<Arc<dyn RunStarter>>,
    /// Runs whose start finished off the world, waiting to be placed in it.
    started_tx: UnboundedSender<starts::Started>,
    started_rx: UnboundedReceiver<starts::Started>,
    /// Starts sent off the world and not yet placed.
    starting: usize,
    reloader: Option<Reloader>,
    force_terminator: Option<ForceTerminator>,
    reaper: Option<Reaper>,
    resumer: Option<Resumer>,
    housekeeper: Option<Housekeeper>,
    events: broadcast::Sender<WorldEvent>,
    emitted: HashMap<String, Emitted>,
    /// The `[limits]` settings the host itself runs on: relief threshold,
    /// listing retention, and the spend figures worth an event. Shared rather
    /// than owned so a config reload can move them without reaching the host -
    /// see [`HostSettings`].
    settings: HostSettings,
    emitted_interactions: HashSet<String>,
    /// Sub-agent world-access requests from tool lanes. The host holds a `tx`
    /// clone so the receiver never closes (its `recv` never yields `None`).
    subagent_tx: UnboundedSender<SubAgentOp>,
    subagent_rx: UnboundedReceiver<SubAgentOp>,
    /// How often [`Self::serve`] re-drives the world even though nothing woke
    /// it: 30 seconds, which tests shorten.
    redrive: Duration,
    /// Consecutive re-drives that found the lanes full and nothing moved. See
    /// [`Self::observe_redrive`].
    dead_cycles: u32,
    /// The progress fingerprint as of the previous re-drive, or `None` before
    /// the first one.
    last_progress: Option<u64>,
    /// Extra tool-lane permits the relief valve has handed out and not yet
    /// reclaimed (see [`Self::decay_relief_if_healthy`]).
    relief_granted: usize,
    /// Consecutive re-drives that found the lane healthy (no dead cycles, no
    /// queue) while relief was outstanding - the decay countdown.
    healthy_cycles: u32,
    /// Runs unloaded recently enough to still be worth reporting, oldest first,
    /// each paired with the unix second it was unloaded. See
    /// [`Self::record_finished`].
    finished: VecDeque<(i64, RunListEntry)>,
    /// Runs the host holds out of the world, by run id, each with its listing
    /// row: paused runs it paged out, and runs that cannot be brought back on
    /// this machine as it stands (see [`Self::hold`]). A parked run's full
    /// state is on disk; `Resume`, `Message` and `Cancel` all page it back
    /// in (see `host::paging`), and [`Self::list`] keeps reporting
    /// it so an operator's `lev ps` view does not change just because the
    /// daemon stopped spending memory on a run nobody is driving.
    parked: HashMap<String, RunListEntry>,
    /// Ops waiting on a run being paged in off the loop, by the run.
    paging: HashMap<String, Vec<paging::Deferred>>,
    /// How each page-in just placed went, for the ops it held while they
    /// are handled.
    paged: HashMap<String, Result<AgentId, NotPlaced>>,
    /// Page-ins that finished off the loop, waiting to be placed.
    paged_tx: UnboundedSender<paging::Paged>,
    paged_rx: UnboundedReceiver<paging::Paged>,
}

/// Consecutive healthy re-drives (no dead cycles, empty tool queue) before the
/// relief-decay valve reclaims one granted permit. Each re-drive is seconds
/// apart, so four of them is a comfortably-over margin - and each further
/// healthy cycle reclaims one more, so a full lane's worth drains in minutes.
const HEALTHY_CYCLES_BEFORE_DECAY: u32 = 4;

/// How often the serve loop re-drives the world on its own.
///
/// The loop is event-driven, so a missed wake anywhere parks it indefinitely -
/// the daemon looks alive while nothing progresses, which reads from outside as
/// hours of frozen agents. This bounds any such wedge to one
/// interval instead of "until something unrelated happens", and gives the lane
/// heartbeat a place to run.
///
/// Deliberately not configurable: it is a correctness backstop, not a tuning
/// knob. A no-op re-drive is one tick over a handful of systems plus an event
/// diff, so at this cadence it costs nothing measurable.
const DEFAULT_REDRIVE_INTERVAL: Duration = Duration::from_secs(30);

/// How many consecutive dead cycles trigger the tool-lane relief valve.
///
/// At the 30-second re-drive that is five minutes of a full lane going nowhere -
/// long enough that ordinary backpressure never reaches it, short enough that a
/// genuinely wedged daemon is not left overnight. Served from
/// `[limits] dead_cycles_before_relief`; `0` disables relief.
pub const DEFAULT_DEAD_CYCLES_BEFORE_RELIEF: u32 = 10;

/// How long a run stays in the listing after the daemon unloads it.
///
/// A terminal agent is unloaded a pass or two after it finishes. With no
/// retention window it leaves the listing at that moment, and a run that died on
/// its first inference is then indistinguishable from one that was never
/// spawned: an external scheduler has nothing to go on but a stopwatch, cannot
/// tell a dead spawn from a slow one, and reverts the work and spawns again.
///
/// Five minutes covers several polls of any scheduler that checks in about once
/// a minute, so a single missed or slow poll does not lose the evidence. It is
/// also what the rest of the daemon already means by "long enough that a hiccup
/// cannot cause it": the dashboard calls a run stale at 300 seconds, and
/// [`DEFAULT_DEAD_CYCLES_BEFORE_RELIEF`] at the 30-second re-drive works out to
/// the same five minutes.
///
/// Served from `[limits] finished_retention_secs`; `0` keeps nothing.
pub const DEFAULT_FINISHED_RETENTION_SECS: u64 = 300;

/// How many unloaded runs [`WorldHost::finished`] holds before the oldest are
/// dropped, whatever the retention window says.
///
/// Not configurable: it is a memory bound, not a tuning knob. A factory that
/// finishes runs faster than this fills the window keeps the most recent ones,
/// which are the ones anyone is still asking about. Set the window shorter to
/// control how much the listing shows; this only stops it growing without end.
const MAX_RETAINED_FINISHED: usize = 256;

impl WorldHost {
    /// Wrap a world with a fresh interaction hub.
    pub fn new(world: PipelineWorld) -> Self {
        Self::with_interactions(world, InteractionHub::new())
    }

    /// Wrap a world with a specific interaction hub - the daemon shares one hub
    /// between the tool service's per-agent backends and this host.
    pub fn with_interactions(mut world: PipelineWorld, interactions: InteractionHub) -> Self {
        // 256, not more: a tokio broadcast ring never shrinks, so every slot a
        // busy period fills stays allocated (holding its event's strings) for
        // the daemon's life. Consumers here are live relays, not replayers -
        // one that falls a full ring behind gets a Lagged skip either way.
        let (events, _) = broadcast::channel(256);
        // Let ECS systems (the persistence drain) push events - per-agent log
        // lines - into the same stream the control transport serves.
        world
            .world_mut()
            .insert_resource(WorldEventSink(events.clone()));
        let (subagent_tx, subagent_rx) = tokio::sync::mpsc::unbounded_channel();
        let (started_tx, started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (paged_tx, paged_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            world,
            by_run_id: HashMap::new(),
            interactions,
            starter: None,
            started_tx,
            started_rx,
            starting: 0,
            reloader: None,
            force_terminator: None,
            reaper: None,
            resumer: None,
            housekeeper: None,
            events,
            emitted: HashMap::new(),
            settings: HostSettings::default(),
            emitted_interactions: HashSet::new(),
            parked: HashMap::new(),
            paging: HashMap::new(),
            paged: HashMap::new(),
            paged_tx,
            paged_rx,
            subagent_tx,
            subagent_rx,
            redrive: DEFAULT_REDRIVE_INTERVAL,
            dead_cycles: 0,
            last_progress: None,
            relief_granted: 0,
            healthy_cycles: 0,
            finished: VecDeque::new(),
        }
    }

    /// A handle on the `[limits]` settings this host runs on, for whatever
    /// keeps them in step with `config.toml`. Every clone reads and writes the
    /// same values, so a change made through one is the change the host sees.
    pub fn settings(&self) -> HostSettings {
        self.settings.clone()
    }
}

// Sections of the former single-file host impl, one per concern. An inherent
// impl may live in any module of the defining crate, so each file below carries
// its own `impl WorldHost` block rather than a trait or a free function.
mod emit;
mod paging;
pub use paging::{PageJob, PlacePage};
mod health;
mod listing;
mod starts;
mod subagents;

impl WorldHost {
    /// Forget the emitted-interaction ids that are no longer pending.
    ///
    /// The hub is keyed by agent id but the emitted set is keyed by request
    /// id, so it is pruned by what is still open. Run once per tick, before
    /// the pending prompts are broadcast, so an id a settled prompt used is
    /// free for the next prompt raised under it.
    fn prune_emitted_interactions(&mut self) {
        let still_open: std::collections::HashSet<String> = self
            .interactions
            .pending()
            .into_iter()
            .map(|(_, req)| req.id)
            .collect();
        self.emitted_interactions
            .retain(|id| still_open.contains(id));
    }
}

impl WorldHost {
    /// Register any agent that exists in the world but is missing from the run-id
    /// map, so the host's view is the world's view.
    ///
    /// Not every agent arrives through a `Spawn` control op: fan-out workers are
    /// built straight into the world by the fan-out spawner, which has no handle
    /// on the host to register them. An unregistered agent is invisible to `list`
    /// (so `lev ps` never showed a worker), never reaped (its sandbox and tool
    /// state leak), and - worst - un-cancellable, because a cancel by its run id
    /// misses the map, falls through to the reloader, and pages a **second** live
    /// entity in from that run's on-disk state while the original keeps running.
    /// Adopting them here is idempotent and keeps a stale mapping from winning:
    /// a registered id whose entity has been despawned is re-pointed.
    fn adopt_unregistered_runs(&mut self) {
        let live: Vec<(String, Entity)> = self
            .world
            .world_mut()
            .query::<(Entity, &RunMetadata)>()
            .iter(self.world.world())
            .map(|(entity, md)| (md.run_id.clone(), entity))
            .collect();
        for (run_id, entity) in live {
            // Straight out of this world's query, so it is ours by construction.
            let agent = self.world.own_agent(entity);
            if self.live_entity(&run_id) != Some(agent) {
                self.by_run_id.insert(run_id, agent);
            }
        }
    }

    /// Whether a paused agent is safe to page out of the world.
    ///
    /// Conservative on purpose - this is the restart-equivalence question, and
    /// only shapes where the answer is a settled "yes" qualify:
    /// - status is `Paused`, and the *persisted* status is too (the watermark
    ///   proves the paused snapshot was dispatched, so disk can rebuild it);
    /// - it is a standalone root: no parent that might address it by entity,
    ///   no children whose links a page-in would have to rebuild;
    /// - no open interaction and no fan-out in flight (a pause that landed
    ///   mid-prompt or mid-split keeps its live machinery).
    fn parkable(&self, entity: Entity, status: &AgentStatus) -> bool {
        if !matches!(status, AgentStatus::Paused) {
            return false;
        }
        // No reloader, no parking: a host that cannot page a run back in
        // (an embedded world, a bare test host) must keep it resident, or
        // "paused" silently becomes "gone".
        if self.reloader.is_none() {
            return false;
        }
        let world = self.world.world();
        let paused_persisted = world
            .get::<crate::pipeline::PersistWatermark>(entity)
            .and_then(|w| w.persisted_status())
            == Some(leviath_core::run_meta::RunStatus::Paused);
        paused_persisted
            && world.get::<crate::components::ParentRef>(entity).is_none()
            && world.get::<SubAgentChildren>(entity).is_none()
            && world.get::<crate::fanout::FanOutWaiting>(entity).is_none()
            && world
                .get::<crate::interaction_points::AwaitingInteractionPoint>(entity)
                .is_none()
            && world.get::<AwaitingInteraction>(entity).is_none()
            // An outstanding provider call is a live, unpersisted continuation
            // just like a blocked `ask`: parking despawns the entity, and the
            // response then lands on a dead one and is dropped on the collect
            // system's stale path. So pausing a run mid-inference without this
            // check throws the call away silently: the run comes back from its
            // page-in as `ReadyToInfer` and pays for the same turn twice.
            //
            // `InFlightWork` covers the call still being out; `HeldInference`
            // covers it having landed and being kept for the resume to apply.
            // Either one keeps the run resident until it is resumed.
            && world.get::<crate::pipeline::InFlightWork>(entity).is_none()
            && world.get::<crate::pipeline::HeldInference>(entity).is_none()
            // A routing choice waiting to be asked is the same kind of live,
            // unpersisted continuation: the edges it has to choose between live
            // only on the component. Page the run out and it comes back
            // `ReadyToInfer`, which re-runs the stage that already answered
            // instead of asking where to go next.
            && world
                .get::<crate::pipeline::AwaitingTransitionChoice>(entity)
                .is_none()
    }

    /// Whether a terminal agent is safe to unload: it has no **live** parent that
    /// might still be waiting on it. True for a root (no `ParentRef`), or when its
    /// parent has been despawned or is itself terminal; false while a non-terminal
    /// parent could still be gating on this child.
    fn no_live_parent(&self, entity: Entity) -> bool {
        let world = self.world.world();
        match world.get::<crate::components::ParentRef>(entity) {
            None => true,
            Some(parent_ref) => match world.get::<AgentState>(parent_ref.parent_entity) {
                None => true,
                Some(state) => crate::pipeline::is_terminal_status(&state.status),
            },
        }
    }

    /// Dollar figures to emit [`WorldEvent::Spend`] at, in any order.
    ///
    /// Sorted and de-duplicated here so the caller can pass a config list as
    /// written. Empty disables the events, which is the default: a run that
    /// nobody asked to be told about costs nothing to watch.
    ///
    /// A threshold is reported once per run, the first time the total passes
    /// it, so a long run does not repeat itself every pass.
    pub fn set_spend_notify_usd(&mut self, thresholds: Vec<f64>) {
        self.settings.set_spend_notify_usd(thresholds);
    }

    /// Install what starts this host's runs. Without one, every `Spawn` and
    /// `ValidateSpawn` is refused.
    pub fn set_starter(&mut self, starter: Arc<dyn RunStarter>) {
        self.starter = Some(starter);
    }

    /// Install the reloader used to page an unloaded run back in on demand.
    /// Without one, an op targeting a run that isn't in memory just misses.
    pub fn set_reloader(&mut self, reloader: Reloader) {
        self.reloader = Some(reloader);
    }

    /// Install the `ForceTerminator` used to terminate a run on disk when the
    /// world cannot hold it. Without one, a cancel that misses in the world and
    /// can't be reloaded just misses.
    pub fn set_force_terminator(&mut self, force_terminator: ForceTerminator) {
        self.force_terminator = Some(force_terminator);
    }

    /// Install the reap hook run just before each terminal agent is despawned,
    /// so the daemon can tear down that agent's sandbox and drop its tool state.
    /// Without one, reaping just despawns the entity (the prior behavior).
    pub fn set_reaper(&mut self, reaper: Reaper) {
        self.reaper = Some(reaper);
    }

    /// Install the hook run when a run starts moving again, so the daemon can
    /// re-read the config layers a person edits to unblock it. Without one,
    /// resuming is only un-pausing (the prior behavior).
    pub fn set_resumer(&mut self, resumer: Resumer) {
        self.resumer = Some(resumer);
    }

    /// Install the hook run on every safety re-drive (see `Housekeeper`).
    pub fn set_housekeeper(&mut self, housekeeper: Housekeeper) {
        self.housekeeper = Some(housekeeper);
    }

    /// Run the housekeeping hook, if one is installed.
    fn housekeep(&mut self) {
        if let Some(hook) = self.housekeeper.as_mut() {
            hook(&mut self.world);
        }
    }

    /// Run the resume hook for `entity`, if one is installed.
    ///
    /// The hook is moved out for the call so it does not borrow `self` while
    /// the world does, and put straight back - the same shape the reaper uses.
    pub(super) fn on_resumed(&mut self, entity: Entity) {
        let mut resumer = self.resumer.take();
        if let Some(resume) = resumer.as_mut() {
            resume(&mut self.world, entity);
        }
        self.resumer = resumer;
    }

    /// Resolve a run id to a live entity, or say why it is not in the world.
    /// An op that names an unloaded run has had it paged in before it is
    /// handled (see `host::paging`), so a run that is still not here is one
    /// the page-in did not place, for the reason it found.
    fn resolve_or_reload(&mut self, run_id: &str) -> Result<AgentId, NotPlaced> {
        if let Some(entity) = self.live_entity(run_id) {
            return Ok(entity);
        }
        self.paged
            .get(run_id)
            .cloned()
            .unwrap_or(Err(NotPlaced::Missing))
    }

    /// Keep a run the daemon could not bring back on this machine in the
    /// listing, by `entry`, until it is brought back or cancelled. Its state
    /// stays on disk as it was, so an op that names it tries again.
    pub fn hold(&mut self, entry: RunListEntry) {
        self.parked.insert(entry.run_id.clone(), entry);
    }

    /// The status of the run `agent_id` in the world, when it is in it.
    fn status_in_world(&self, agent_id: &str) -> Option<AgentStatus> {
        self.world
            .world()
            .iter_entities()
            .filter_map(|e| e.get::<AgentState>())
            .find(|s| s.agent_id == agent_id)
            .map(|s| s.status.clone())
    }

    /// Why a message to `agent_id`, a run with `status` (`None` for no such
    /// run), would never be read, when it would not. A message is only said
    /// to be delivered when a run will read it.
    fn undeliverable(agent_id: &str, status: Option<&AgentStatus>) -> Option<String> {
        match status {
            None => Some(format!(
                "no run '{agent_id}' is here to read a message; `lev ps --all` lists the runs"
            )),
            Some(AgentStatus::Complete) => Some(format!(
                "run '{agent_id}' has finished, so it reads no more messages; start a new run instead"
            )),
            Some(AgentStatus::Error { .. }) => Some(format!(
                "run '{agent_id}' has failed, so it reads no more messages; start a new run instead"
            )),
            Some(AgentStatus::Cancelled) => Some(format!(
                "run '{agent_id}' was cancelled; `lev resume {agent_id}` first, then send the message"
            )),
            Some(_) => None,
        }
    }

    /// Why a message to a run held out of the world is not delivered: what
    /// has to change on this machine first.
    fn held_refusal(agent_id: &str, entry: &RunListEntry) -> String {
        format!(
            "run '{agent_id}' cannot go on on this machine as it stands, so it reads no messages yet: {}",
            Self::held_remedy(entry)
        )
    }

    /// Why a held run did not resume: this machine still cannot take it
    /// back, and what to put back first.
    pub(super) fn held_resume_refusal(run_id: &str, entry: &RunListEntry) -> String {
        format!(
            "run '{run_id}' cannot go on on this machine as it stands, so it stays paused: {}",
            Self::held_remedy(entry)
        )
    }

    /// What a held run's listing row says to put back.
    fn held_remedy(entry: &RunListEntry) -> &str {
        match &entry.wait_reason {
            Some(WaitReason::NeedsSetup { remedy, .. }) => remedy.as_str(),
            _ => "",
        }
    }

    /// A clone of the interaction hub, for building per-agent backends.
    #[cfg(test)]
    pub(crate) fn interactions(&self) -> InteractionHub {
        self.interactions.clone()
    }

    /// Mutable access to the underlying world (for the spawner to add agents).
    pub fn world_mut(&mut self) -> &mut PipelineWorld {
        &mut self.world
    }

    /// Record the run-id → entity mapping for a freshly-spawned agent.
    pub fn register(&mut self, run_id: impl Into<String>, agent: AgentId) {
        let run_id = run_id.into();
        self.parked.remove(&run_id);
        self.by_run_id.insert(run_id, agent);
    }

    /// Resolve a run id to a **live** entity (one that still exists in the world).
    fn live_entity(&self, run_id: &str) -> Option<AgentId> {
        let agent = *self.by_run_id.get(run_id)?;
        self.world
            .world()
            .get::<AgentState>(agent.entity())
            .map(|_| agent)
    }

    /// Apply one control op and reply on its channel, once the run it names
    /// is in the world (see `host::paging`). A dropped reply receiver is
    /// harmless (the requester went away).
    pub fn handle(&mut self, op: ControlOp) {
        self.page_first(paging::Deferred::Control(op));
    }

    /// Apply one control op whose run, if it names one, has been paged in.
    fn handle_now(&mut self, op: ControlOp) {
        match op {
            ControlOp::Spawn { request, reply } => {
                self.start(*request, crate::spec::env::Caller::TopLevel, None, reply);
            }
            ControlOp::ValidateSpawn { request, reply } => {
                self.validate(*request, crate::spec::env::Caller::TopLevel, reply);
            }
            ControlOp::Inspect { run_id, reply } => {
                let _ = reply.send(self.inspect(&run_id).map(Box::new));
            }
            ControlOp::Result { run_id, reply } => {
                // Live entities only. An unloaded run's answer is on disk in
                // its `final_output` sidecar, which is what `lev result`
                // reads; keeping a copy of every finished run's answer in
                // memory would defeat the point of bounding the finished
                // buffer.
                let output = self
                    .live_entity(&run_id)
                    .and_then(|agent| {
                        self.world
                            .world()
                            .get::<crate::persistence::FinalOutput>(agent.entity())
                    })
                    .map(|o| o.0.clone());
                let _ = reply.send(output);
            }
            ControlOp::Blob {
                run_id,
                sha256,
                reply,
            } => {
                // The store is content-addressed per run and outlives the
                // entity, so a finished run's artifact is still readable here
                // after the daemon has unloaded it.
                let bytes = self
                    .world
                    .world()
                    .get_resource::<crate::blob_store::BlobStoreHandle>()
                    .and_then(|store| store.0.read(&run_id, &sha256).ok())
                    .map(|bytes| bytes.to_vec());
                let _ = reply.send(bytes);
            }
            ControlOp::Status { run_id, reply } => {
                // A run the daemon has unloaded still has an answer for a
                // while, so a caller that asks a moment too late learns how the
                // run ended instead of being told there is no such run.
                let status = self
                    .live_entity(&run_id)
                    .and_then(|e| self.world.agent_status(e))
                    .or_else(|| self.parked.get(&run_id).map(|e| e.status.clone()))
                    .or_else(|| {
                        self.finished
                            .iter()
                            .find(|(_, e)| e.run_id == run_id)
                            .map(|(_, e)| e.status.clone())
                    });
                let _ = reply.send(status);
            }
            // Both walk the sub-agent tree. Pausing only the run that was named
            // leaves a fan-out parent's children running - the parent is
            // `Waiting`, which is not pausable, so the request would report
            // failure while the work carried on - and resuming only the parent
            // would strand every child the pause had stopped.
            ControlOp::Pause { run_id, reply } => {
                let ok = self.pause_tree(&run_id);
                let _ = reply.send(ok);
            }
            ControlOp::Resume { run_id, reply } => {
                let _ = reply.send(self.resume_tree(&run_id));
            }
            ControlOp::Cancel { run_id, reply } => {
                // Cancel is unconditional: it either takes effect in the world
                // (root plus every descendant) or, when the run can't be held
                // there at all, is forced onto its on-disk state. It reports
                // `false` only when there is genuinely no such run anywhere -
                // otherwise a run whose blueprint had moved stayed `running` on
                // disk forever with no way to get rid of it.
                let ok = self.cancel_tree(&run_id) || self.force_cancel(&run_id);
                let _ = reply.send(ok);
            }
            ControlOp::List { reply } => {
                let _ = reply.send(RunListing {
                    runs: self.list(),
                    finished: self.finished(),
                    health: self.health(),
                });
            }
            ControlOp::Message {
                agent_id,
                content,
                target_region,
                parts,
                reply,
            } => {
                // A message with no words and no files wakes the run for an
                // empty turn and costs it an inference, for nothing anybody
                // said.
                if content.trim().is_empty() && parts.is_empty() {
                    let _ = reply.send(Err(
                        "refusing to deliver an empty message: give it some text or a file"
                            .to_string(),
                    ));
                    return;
                }
                // Page the target in if it was unloaded, so delivery finds it.
                // A run that has stopped stays stopped: it is not loaded back
                // for a message it would never read.
                let status = match self.resolve_or_reload(&agent_id) {
                    Err(NotPlaced::Stopped(status)) => Some(status),
                    Err(NotPlaced::Held(entry)) => {
                        let _ = reply.send(Err(Self::held_refusal(&agent_id, &entry)));
                        return;
                    }
                    Ok(_) | Err(NotPlaced::Missing) => self.status_in_world(&agent_id),
                };
                if let Some(why) = Self::undeliverable(&agent_id, status.as_ref()) {
                    let _ = reply.send(Err(why));
                    return;
                }
                let ok = self
                    .world
                    .send_message(AgentMessage {
                        agent_id,
                        from: crate::components::FROM_PERSON.to_string(),
                        content,
                        target_region,
                        parts,
                    })
                    .is_ok();
                let _ = reply.send(Ok(ok));
            }
            ControlOp::ListInteractions { reply } => {
                let _ = reply.send(self.interactions.pending());
            }
            ControlOp::AnswerInteraction { response, reply } => {
                // Answering a prompt is one of the points a run resumes at: the
                // person who just said "no, and here is why" may equally have
                // gone and changed the permission the prompt was about, and
                // this is where that reaches the run (an unanswered prompt
                // waits for ever, so a run stuck behind one has no other way
                // out but a cancel).
                let outcome = match self.interactions.answer_for(response) {
                    Ok(agent) => {
                        if let Some(entity) = self.live_entity(&agent) {
                            self.on_resumed(entity.entity());
                        }
                        Ok(true)
                    }
                    Err(AnswerError::NotOpen) => Ok(false),
                    Err(AnswerError::Refused(why)) => Err(why),
                };
                let _ = reply.send(outcome);
            }
            ControlOp::CancelInteraction { request_id, reply } => {
                let _ = reply.send(self.interactions.cancel(&request_id));
            }
            ControlOp::Shutdown { reply } => {
                // Reply first (best effort), then trigger the world's shutdown so
                // the serve loop's next `select!` returns.
                let _ = reply.send(true);
                self.world.shutdown();
            }
        }
    }

    /// Flush all queued persistence and stop the hosted world, guaranteeing every
    /// dirty agent's final snapshot reaches disk (see
    /// [`PipelineWorld::flush_and_stop`]). Invoked automatically when [`Self::serve`]
    /// returns; also exposed directly for callers that drive the world themselves.
    pub(crate) async fn flush_and_stop(&mut self) {
        self.world.flush_and_stop().await;
    }

    /// Run the host: drive the world to quiescence, then park until an async
    /// result wakes it, a control op arrives, or shutdown is signalled. Returns
    /// when shutdown fires, or once the control channel has closed and every
    /// run it asked for has been placed or refused (so each of their replies
    /// is sent) - and before returning,
    /// **flushes all queued persistence to disk** (`Self::flush_and_stop`) so a
    /// clean daemon shutdown never loses a dirty agent's final snapshot.
    pub async fn serve(&mut self, mut control_rx: UnboundedReceiver<ControlOp>) {
        let wake = self.world.wake_handle();
        let shutdown = self.world.shutdown_handle();
        // `interval_at` rather than `interval`: the latter's first tick is
        // immediately ready, which would spin one pointless pass at startup.
        // `Delay` keeps a slow drive from queueing a burst of catch-up ticks.
        let mut redrive =
            tokio::time::interval_at(tokio::time::Instant::now() + self.redrive, self.redrive);
        redrive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut closed = false;
        'serve: loop {
            if closed && self.starting == 0 && self.paging.is_empty() {
                break 'serve;
            }
            self.world.run_to_fixed_point();
            self.emit_events();
            tokio::select! {
                _ = wake.notified() => {}
                _ = shutdown.notified() => break 'serve,
                // The backstop. Everything else here is edge-triggered, so a
                // release or completion that forgets to wake us would otherwise
                // park the daemon indefinitely with work left to do. Re-driving
                // on a timer bounds that to one interval, and is where the lane
                // heartbeat reports what the loop is actually waiting on.
                _ = redrive.tick() => {
                    self.observe_redrive();
                    self.housekeep();
                }
                op = control_rx.recv(), if !closed => {
                    match op {
                        Some(op) => self.handle(op),
                        // All control senders dropped: finish the starts in
                        // flight, then stop.
                        None => closed = true,
                    }
                }
                // The host holds a `started_tx`, so this only yields `Some`.
                Some(started) = self.started_rx.recv() => self.place(started),
                // The host holds a `subagent_tx`, so this only yields `Some`.
                Some(sub) = self.subagent_rx.recv() => self.handle_subagent(sub),
                // And a `paged_tx`.
                Some(paged) = self.paged_rx.recv() => self.landed_page(paged),
            }
        }
        // Shutting down: drain the persistence lane before the world is dropped.
        self.flush_and_stop().await;
    }
}

#[cfg(test)]
#[path = "../host_tests.rs"]
mod tests;
