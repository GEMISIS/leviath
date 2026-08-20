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
use std::time::Duration;

use bevy_ecs::entity::Entity;
use tokio::sync::broadcast;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::components::{
    AgentMessage, AgentState, AgentStatus, AwaitingInteraction, ContextWindow, ParentRef,
    SubAgentChildren, WaitReason,
};
use crate::interaction_hub::InteractionHub;
use crate::persistence::{RunMetadata, TokenTotals};
use crate::world::{AgentId, LaneSnapshot, PipelineWorld};

// Sections of the former single-file host, one per concern. Glob re-exported so
// every existing `host::ControlOp` / `host::WorldEvent` path keeps working and
// the split stays a pure move.
mod events;
pub use events::*;
mod types;
pub use types::*;

/// Owns the world and the run-id map; drives the world and services control ops.
pub struct WorldHost {
    world: PipelineWorld,
    by_run_id: HashMap<String, AgentId>,
    interactions: InteractionHub,
    spawner: Option<Spawner>,
    spawn_preprocessor: Option<SpawnPreprocessor>,
    reloader: Option<Reloader>,
    force_terminator: Option<ForceTerminator>,
    reaper: Option<Reaper>,
    events: broadcast::Sender<WorldEvent>,
    emitted: HashMap<String, Emitted>,
    emitted_interactions: HashSet<String>,
    /// Sub-agent world-access requests from tool lanes. The host holds a `tx`
    /// clone so the receiver never closes (its `recv` never yields `None`).
    subagent_tx: UnboundedSender<SubAgentOp>,
    subagent_rx: UnboundedReceiver<SubAgentOp>,
    /// How often [`Self::serve`] re-drives the world even though nothing woke
    /// it. See [`Self::set_redrive_interval`].
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
    /// Dead cycles the daemon tolerates before widening the tool lane. `0`
    /// disables relief. See [`Self::set_dead_cycles_before_relief`].
    dead_cycles_before_relief: u32,
    /// Runs unloaded recently enough to still be worth reporting, oldest first,
    /// each paired with the unix second it was unloaded. See
    /// [`Self::record_finished`].
    finished: VecDeque<(i64, RunListEntry)>,
    /// How long an unloaded run stays in [`Self::finished`]. `0` keeps none.
    /// See [`Self::set_finished_retention_secs`].
    finished_retention_secs: u64,
    /// Paused runs the host has paged out of the world, by run id, each holding
    /// its last listing row. A parked run's full state is on disk; `Resume`,
    /// `Message` and `Cancel` all page it back through
    /// [`Self::resolve_or_reload`], and [`Self::list`] keeps reporting it so an
    /// operator's `lev ps` view does not change just because the daemon stopped
    /// spending memory on a run nobody is driving.
    parked: HashMap<String, RunListEntry>,
}

/// Consecutive healthy re-drives (no dead cycles, empty tool queue) before the
/// relief-decay valve reclaims one granted permit. Each re-drive is seconds
/// apart, so four of them is a comfortably-over margin - and each further
/// healthy cycle reclaims one more, so a full lane's worth drains in minutes.
const HEALTHY_CYCLES_BEFORE_DECAY: u32 = 4;

/// How often the serve loop re-drives the world on its own.
///
/// The loop is event-driven, so a missed wake anywhere parks it indefinitely -
/// the daemon looks alive while nothing progresses, which is what issue #189
/// reported as hours of frozen agents. This bounds any such wedge to one
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
/// A terminal agent is unloaded a pass or two after it finishes, and until now
/// it vanished from the listing at that moment. A run that died on its first
/// inference was therefore indistinguishable from one that had never been
/// spawned, which is what left the scheduler in issue #205 with nothing to go on
/// but a stopwatch: it could not tell a dead spawn from a slow one, so it
/// reverted the work and spawned again, for forty minutes.
///
/// Five minutes covers several polls of any scheduler that checks in about once
/// a minute, so a single missed or slow poll does not lose the evidence. It is
/// also what the rest of the daemon already means by "long enough that a hiccup
/// cannot cause it": the dashboard calls a run stale at 300 seconds, and
/// [`DEFAULT_DEAD_CYCLES_BEFORE_RELIEF`] at the 30-second re-drive works out to
/// the same five minutes.
///
/// Served from `[limits] finished_retention_secs`; `0` keeps nothing and
/// restores the old behaviour.
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
        Self {
            world,
            by_run_id: HashMap::new(),
            interactions,
            spawner: None,
            spawn_preprocessor: None,
            reloader: None,
            force_terminator: None,
            reaper: None,
            events,
            emitted: HashMap::new(),
            emitted_interactions: HashSet::new(),
            parked: HashMap::new(),
            subagent_tx,
            subagent_rx,
            redrive: DEFAULT_REDRIVE_INTERVAL,
            dead_cycles: 0,
            last_progress: None,
            relief_granted: 0,
            healthy_cycles: 0,
            dead_cycles_before_relief: DEFAULT_DEAD_CYCLES_BEFORE_RELIEF,
            finished: VecDeque::new(),
            finished_retention_secs: DEFAULT_FINISHED_RETENTION_SECS,
        }
    }
}

// Sections of the former single-file host impl, one per concern. An inherent
// impl may live in any module of the defining crate, so each file below carries
// its own `impl WorldHost` block rather than a trait or a free function.
mod emit;
mod health;
mod listing;
mod subagents;

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
            // system's stale path. So pausing a run mid-inference used to throw
            // the call away silently - the run came back from its page-in as
            // `ReadyToInfer` and paid for the same turn twice.
            //
            // `InFlightWork` covers the call still being out; `HeldInference`
            // covers it having landed and being kept for the resume to apply.
            // Either one keeps the run resident until it is resumed.
            && world.get::<crate::pipeline::InFlightWork>(entity).is_none()
            && world.get::<crate::pipeline::HeldInference>(entity).is_none()
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
                Some(state) => matches!(
                    state.status,
                    AgentStatus::Complete | AgentStatus::Error { .. } | AgentStatus::Cancelled
                ),
            },
        }
    }

    /// Install the spawner used to service `Spawn` control ops. Without one, a
    /// `Spawn` op replies with an error.
    pub fn set_spawner(&mut self, spawner: Spawner) {
        self.spawner = Some(spawner);
    }

    /// Install the async hook awaited before each top-level `Spawn` (see
    /// [`SpawnPreprocessor`]).
    pub fn set_spawn_preprocessor(&mut self, pp: SpawnPreprocessor) {
        self.spawn_preprocessor = Some(pp);
    }

    /// Install the reloader used to page an unloaded run back in on demand.
    /// Without one, an op targeting a run that isn't in memory just misses.
    pub fn set_reloader(&mut self, reloader: Reloader) {
        self.reloader = Some(reloader);
    }

    /// Install the [`ForceTerminator`] used to terminate a run on disk when the
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

    /// Resolve a run id to a live entity, paging it in from disk if it has been
    /// unloaded (and a reloader is installed). Returns `None` if the run is
    /// neither live nor resumable from disk. Newly-reloaded runs are registered.
    fn resolve_or_reload(&mut self, run_id: &str) -> Option<AgentId> {
        if let Some(entity) = self.live_entity(run_id) {
            return Some(entity);
        }
        let entity = (self.reloader.as_mut()?)(&mut self.world, run_id)?;
        self.by_run_id.insert(run_id.to_string(), entity);
        // Live again: its listing row comes off the entity, not the parked map.
        self.parked.remove(run_id);
        Some(entity)
    }

    /// A clone of the interaction hub, for building per-agent backends.
    pub fn interactions(&self) -> InteractionHub {
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

    /// Apply one control op and reply on its channel. A dropped reply receiver is
    /// harmless (the requester went away).
    pub fn handle(&mut self, op: ControlOp) {
        match op {
            ControlOp::Spawn { args, reply } => {
                let result = match self.spawner.as_mut() {
                    // Spawning runs outside the pipeline schedule, so it isn't
                    // covered by `run_isolated`'s panic guard: a panic while
                    // parsing a blueprint or building a sandbox would otherwise
                    // unwind the whole serve task and take the daemon with it.
                    // As with `run_isolated`, the world may be left holding a
                    // partially-built entity - the run just never registers.
                    Some(spawner) => {
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            spawner(&mut self.world, &args)
                        })) {
                            Ok(Ok(entity)) => {
                                // Spawned into this world, so it is ours.
                                let agent = self.world.own_agent(entity);
                                self.by_run_id.insert(args.run_id.clone(), agent);
                                Ok(args.run_id.clone())
                            }
                            Ok(Err(e)) => Err(e),
                            Err(_) => Err("agent spawn panicked".to_string()),
                        }
                    }
                    None => Err("this daemon cannot spawn agents".to_string()),
                };
                // A failed spawn must leave a trace daemon-side: the error goes
                // back over the socket to a client that may have already exited,
                // and nothing is written to disk, so without this log line the
                // failure is invisible (issue #107).
                if let Err(error) = &result {
                    tracing::error!(
                        run_id = %args.run_id,
                        blueprint = %args.blueprint_path,
                        workdir = %args.workdir,
                        error = %error,
                        "agent spawn failed"
                    );
                }
                let _ = reply.send(result);
            }
            ControlOp::Result { run_id, reply } => {
                // Live entities only. An unloaded run's answer is on disk in
                // `meta.json`, which is what `lev result` reads; keeping a copy
                // of every finished run's answer in memory would defeat the
                // point of bounding the finished buffer.
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
                let ok = self.resume_tree(&run_id);
                let _ = reply.send(ok);
            }
            ControlOp::Cancel { run_id, reply } => {
                // Cancel is unconditional: it either takes effect in the world
                // (root plus every descendant) or, when the run can't be held
                // there at all, is forced onto its on-disk state. It reports
                // `false` only when there is genuinely no such run anywhere -
                // otherwise a run whose blueprint had moved stayed `running` on
                // disk forever with no way to get rid of it.
                let ok = self.cancel_tree(&run_id)
                    || self
                        .force_terminator
                        .as_mut()
                        .is_some_and(|terminate| terminate(&run_id));
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
                reply,
            } => {
                // Page the target in if it was unloaded, so delivery finds it.
                self.resolve_or_reload(&agent_id);
                let ok = self
                    .world
                    .send_message(AgentMessage {
                        agent_id,
                        content,
                        target_region,
                    })
                    .is_ok();
                let _ = reply.send(ok);
            }
            ControlOp::ListInteractions { reply } => {
                let _ = reply.send(self.interactions.pending());
            }
            ControlOp::AnswerInteraction { response, reply } => {
                let _ = reply.send(self.interactions.answer(response));
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
    pub async fn flush_and_stop(&mut self) {
        self.world.flush_and_stop().await;
    }

    /// Run the host: drive the world to quiescence, then park until an async
    /// result wakes it, a control op arrives, or shutdown is signalled. Returns
    /// when shutdown fires or the control channel closes - and before returning,
    /// **flushes all queued persistence to disk** ([`Self::flush_and_stop`]) so a
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
        'serve: loop {
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
                _ = redrive.tick() => self.observe_redrive(),
                op = control_rx.recv() => {
                    match op {
                        // Await the spawn preprocessor (e.g. lazy MCP connect) before
                        // the sync spawner runs, so the pool is warm. The returned
                        // future is `'static`, so no borrow of `self`/`op` outlives it.
                        Some(op) => {
                            let pre = match &op {
                                ControlOp::Spawn { args, .. } => {
                                    self.spawn_preprocessor.as_ref().map(|pp| pp(args))
                                }
                                _ => None,
                            };
                            if let Some(fut) = pre {
                                fut.await;
                            }
                            self.handle(op);
                        }
                        None => break 'serve, // all control senders dropped
                    }
                }
                // The host holds a `subagent_tx`, so this only yields `Some`.
                Some(sub) = self.subagent_rx.recv() => {
                    // Warm a spawning sub-agent's MCP servers first, same as a
                    // top-level Spawn (both run in this async loop).
                    let pre = match &sub {
                        SubAgentOp::Spawn { args, .. } => {
                            self.spawn_preprocessor.as_ref().map(|pp| pp(args))
                        }
                        _ => None,
                    };
                    if let Some(fut) = pre {
                        fut.await;
                    }
                    self.handle_subagent(sub);
                }
            }
        }
        // Shutting down: drain the persistence lane before the world is dropped.
        self.flush_and_stop().await;
    }
}

#[cfg(test)]
#[path = "../host_tests.rs"]
mod tests;
