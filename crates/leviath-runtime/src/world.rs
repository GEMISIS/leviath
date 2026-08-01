//! The pipeline driver: a single [`PipelineWorld`] that hosts every agent as
//! ECS data and ticks the [`crate::pipeline`] systems over all of them - the
//! traditional-game-loop core of the shared world.
//!
//! The world owns the bevy [`World`], the tick [`Schedule`], the per-model
//! inference pools, and the async bridges (inference jobs + the tool worker).
//! Systems never block: they dispatch async work to the bridges and collect the
//! results on a later tick. Between ticks the driver **parks** on a wake
//! [`Notify`] until an async result lands or an external message arrives, so an
//! idle world costs ~0 CPU regardless of how many (paused/blocked) agents it
//! holds.
//!
//! ## Idle detection (no busy-spin)
//!
//! Each outer iteration drives the schedule to a **fixed point**: it ticks until
//! a tick produces no change in the per-phase marker counts (the "fingerprint").
//! At quiescence every remaining agent is either waiting on an in-flight async
//! job (which will `notify` on completion) or blocked on a resource that only an
//! async completion can free (a full pool) or on nothing at all (a missing
//! provider / no input) - so the driver parks on the wake instead of spinning.
//! A fresh async result or an external `send_message` fires the wake and the
//! fixed-point loop re-runs.

use std::sync::Arc;

use bevy_ecs::prelude::*;
use bevy_ecs::query::QueryFilter;
use leviath_providers::ProviderError;
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

use crate::components::{AgentMessage, AgentState, AgentStatus};
use crate::inference_pool::{InferencePoolConfig, InferencePools};
use crate::persistence_bridge::persistence_worker;
use crate::pipeline::{
    AwaitingCompaction, AwaitingInference, AwaitingTools, AwaitingTransitionChoice,
    AwaitingTransitionResponse, CompactionResults, InferenceResults, InferenceStage, MessageIntake,
    PersistenceStage, ProcessResponse, Providers, ReadyForTools, ReadyForTransition, ReadyToInfer,
    ResolveTransition, ToolResults, ToolService, ToolServiceRes, ToolStage, TransitionResults,
    abort_terminal_work, check_workspace_health, collect_compaction, collect_inference,
    collect_tools, collect_transition_choice, deliver_messages, detect_stuck_stage,
    dispatch_compaction, dispatch_edge_compact, dispatch_inference, dispatch_persistence,
    dispatch_tools, dispatch_transition_choice, enforce_max_iterations, gate_requires_children,
    handle_empty_response, poll_dynamic_tool_refresh, process_response, reflect_interaction_status,
    refresh_advertised_tools, require_context_regions, resolve_transition, sync_tool_stages,
};
use crate::providers::ProviderRegistry;
use crate::tool_bridge::spawn_tool_pool;

/// Counts of agents in each phase-marker - the world's per-tick "fingerprint".
/// Two consecutive equal fingerprints mean a tick changed nothing (quiescence).
type Fingerprint = [usize; 12];

/// How many attributed system panics one [`PipelineWorld::run_to_fixed_point`]
/// round will absorb before it stops driving. Each one fails a different agent,
/// so this only bites if the world is thoroughly broken - it exists so a
/// pathological agent can't spin the loop.
const MAX_TICK_FAILURES_PER_ROUND: usize = 8;

/// A schedule configured the way the pipeline needs it.
///
/// Every pipeline system is `.chain()`ed, so the multi-threaded executor can
/// never overlap two of them - it only adds a hop through the compute task
/// pool. Running single-threaded keeps systems on the thread that catches their
/// panics, which is what lets [`run_isolated`] read the offending agent out of
/// the (thread-local) [`crate::tick_scope`].
fn tick_schedule() -> Schedule {
    let mut schedule = Schedule::default();
    // bevy_ecs 0.19 replaced `set_executor_kind(ExecutorKind::…)` with
    // `set_executor(<executor instance>)`.
    schedule.set_executor(bevy_ecs::schedule::SingleThreadedExecutor::new());
    schedule
}

/// What one [`PipelineWorld::tick`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickOutcome {
    /// Every system ran to completion.
    Clean,
    /// A system panicked and the agent responsible was failed; the rest of the
    /// world is unaffected and can keep being driven.
    AgentFailed,
    /// A system panicked with no agent in scope, so nothing could be failed.
    /// Re-ticking would just re-panic.
    Unattributed,
}

/// The shared ECS world that hosts and drives every agent.
pub struct PipelineWorld {
    world: World,
    schedule: Schedule,
    wake: Arc<Notify>,
    shutdown: Arc<Notify>,
    msg_tx: UnboundedSender<AgentMessage>,
    /// The tool worker pool tasks; kept so they live as long as the world. Each
    /// exits on its own when the world (and thus the [`ToolStage`] sender) is
    /// dropped. The pool size is the tool-lane concurrency cap.
    _tool_tasks: Vec<JoinHandle<()>>,
    /// The persistence worker task. Retained (rather than detached) so
    /// [`Self::flush_and_stop`] can close its channel and `await` it, guaranteeing
    /// every queued snapshot reaches disk before shutdown. `None` once flushed.
    persist_task: Option<JoinHandle<()>>,
}

impl PipelineWorld {
    /// Build a world: wire the pool/bridge resources, register the providers and
    /// tool service, spawn the tool worker onto `runtime`, and assemble the tick
    /// schedule. Agents are added later via [`Self::spawn_agent`].
    pub fn new(
        providers: ProviderRegistry,
        tool_service: Arc<dyn ToolService>,
        pool_config: InferencePoolConfig,
        tool_concurrency: usize,
        runs_dir: std::path::PathBuf,
        runtime: Handle,
    ) -> Self {
        // `Query::par_iter` fans out over the compute task pool; initialize it
        // once (idempotent) so per-agent request assembly in `dispatch_inference`
        // runs in parallel. (The schedule executor itself is single-threaded -
        // see `tick_schedule`.)
        bevy_tasks::ComputeTaskPool::get_or_init(bevy_tasks::TaskPool::default);

        let wake = Arc::new(Notify::new());
        let shutdown = Arc::new(Notify::new());

        let (inf_tx, inf_rx) = unbounded_channel();
        let (trans_tx, trans_rx) = unbounded_channel();
        let (compact_tx, compact_rx) = unbounded_channel();
        let (tool_job_tx, tool_job_rx) = unbounded_channel();
        let (tool_res_tx, tool_res_rx) = unbounded_channel();
        let (persist_tx, persist_rx) = unbounded_channel();
        let (msg_tx, msg_rx) = unbounded_channel();
        let (ip_tx, ip_rx) = unbounded_channel();
        let (gp_tx, gp_rx) = unbounded_channel();
        let (cs_tx, cs_rx) = unbounded_channel();
        let (title_tx, title_rx) = unbounded_channel();

        let tool_tasks = spawn_tool_pool(
            &runtime,
            tool_job_rx,
            tool_res_tx,
            wake.clone(),
            tool_concurrency,
        );
        // Retained so `flush_and_stop` can drain it on shutdown. Left to its own
        // devices otherwise: it exits when the world (and thus its PersistenceStage
        // sender) is dropped.
        let persist_task = runtime.spawn(persistence_worker(runs_dir, persist_rx));
        let ip_runtime = runtime.clone();
        let gp_runtime = runtime.clone();

        let mut world = World::new();
        world.insert_resource(Providers(providers));
        world.insert_resource(crate::ContentInternerRes::new());
        world.insert_resource(InferenceStage {
            pools: Arc::new(InferencePools::new(pool_config)),
            outcomes: inf_tx,
            transition_outcomes: trans_tx,
            compaction_outcomes: compact_tx,
            content_summary_outcomes: cs_tx,
            wake: wake.clone(),
            runtime,
            exact_token_counting: false,
        });
        world.insert_resource(crate::context_transform::ContentSummaryResults(cs_rx));
        world.insert_resource(crate::title::TitleSink(title_tx));
        world.insert_resource(crate::title::TitleResults(title_rx));
        world.insert_resource(crate::interaction_points::InteractionPointStage {
            outcomes: ip_tx,
            wake: wake.clone(),
            runtime: ip_runtime,
        });
        world.insert_resource(crate::interaction_points::InteractionPointResults(ip_rx));
        world.insert_resource(crate::gate_prompt::GatePromptStage {
            outcomes: gp_tx,
            wake: wake.clone(),
            runtime: gp_runtime,
        });
        world.insert_resource(crate::gate_prompt::GatePromptResults(gp_rx));
        world.insert_resource(InferenceResults(inf_rx));
        world.insert_resource(TransitionResults(trans_rx));
        world.insert_resource(CompactionResults(compact_rx));
        world.insert_resource(ToolServiceRes(tool_service));
        world.insert_resource(ToolStage(tool_job_tx));
        world.insert_resource(ToolResults(tool_res_rx));
        world.insert_resource(PersistenceStage(persist_tx));
        world.insert_resource(MessageIntake(msg_rx));
        // Telemetry defaults to the no-op sink; a host that wants export
        // replaces the resource after construction (as `build_host` does).
        world.insert_resource(crate::telemetry::Telemetry(std::sync::Arc::new(
            leviath_core::telemetry::NoopSink,
        )));

        // The tick chain is split into two `.chain()`ed groups (bevy caps a
        // system tuple at 20); the second group runs strictly after the first.
        let mut schedule = tick_schedule();
        schedule.add_systems(
            (
                // First: stop whatever a now-terminal agent still has running in
                // the async lanes. Ahead of everything else so a cancel frees its
                // inference permit and tool-lane worker on the very next tick,
                // rather than whenever the provider or tool happens to answer.
                abort_terminal_work,
                deliver_messages,
                collect_compaction,
                // Apply any completed Summarize context-transform summaries into
                // the child's regions, then dispatch newly-queued ones.
                crate::context_transform::collect_content_summary,
                crate::context_transform::dispatch_content_summary,
                // Route edge-transform compaction through the compaction lane
                // before the threshold-based pass.
                dispatch_edge_compact,
                dispatch_compaction,
                // Cap a stage at its max_iterations before running more inference.
                enforce_max_iterations,
                // …then the softer guard: bail out of a stage that is burning
                // turns/edits without progress, when the blueprint declares a
                // `stuck` escape edge. Runs after the hard cap so that always wins.
                detect_stuck_stage,
                // Stop a run whose working directory vanished, rather than let
                // every tool fail with ENOENT for the rest of the run.
                check_workspace_health,
                // Tag dynamic_tools agents that have pending tool changes, then
                // apply the re-advertisement before the next request is assembled
                // so a newly-discovered tool is visible.
                poll_dynamic_tool_refresh,
                refresh_advertised_tools,
                dispatch_inference,
                collect_inference,
                // Intercept a fan-out stage's split response before normal routing.
                crate::fanout::fan_out_split,
                process_response,
                // Apply resolved taint gate prompts (re-arming ReadyForTools)
                // before the tool dispatch re-runs the held batch.
                crate::gate_prompt::collect_gate_prompt,
                dispatch_tools,
                collect_tools,
                // Apply any resolved stage-boundary interaction-point answers
                // before the stage decides its transition.
                crate::interaction_points::collect_interaction_point,
            )
                .chain(),
        );
        schedule.add_systems(
            (
                handle_empty_response,
                // Hold a `requires_children` stage until its sub-agents finish.
                gate_requires_children,
                // Re-run a stage that left a `required` context region empty
                // before it may transition or ask for approval.
                require_context_regions,
                // Intercept a would-be transition for an interactive-points stage
                // (e.g. plan_approval) and drive the interaction-point lane.
                crate::interaction_points::gate_interaction_points,
                crate::interaction_points::dispatch_interaction_point,
                resolve_transition,
                dispatch_transition_choice,
                collect_transition_choice,
                // Drive fan-out workers and merge once they finish.
                crate::fanout::fan_out_collect,
                // Narrate lifecycle/activity into the telemetry sink. Must run
                // before `sync_tool_stages` (which consumes the transient
                // `StageJustEntered` marker) and before `dispatch_persistence`
                // (which drains the log buffer this system only reads).
                crate::telemetry::observe_lifecycle,
                sync_tool_stages,
                // Store any finished run title, then start newly-marked ones.
                // Collect precedes persistence so a landed title is written on
                // this same tick.
                crate::title::collect_title,
                crate::title::dispatch_title,
                // Mirror open interaction-hub requests into agent status
                // (Active ↔ Waiting) so the dashboard surfaces blocked prompts;
                // must run before persistence so the status change is written.
                reflect_interaction_status,
                dispatch_persistence,
            )
                .chain()
                .after(crate::interaction_points::collect_interaction_point),
        );

        Self {
            world,
            schedule,
            wake,
            shutdown,
            msg_tx,
            _tool_tasks: tool_tasks,
            persist_task: Some(persist_task),
        }
    }

    /// Mutable access to the underlying ECS world, for spawning agents (the CLI /
    /// daemon builds each agent's component bundle) and inspection.
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// Read-only access to the underlying ECS world.
    pub fn world(&self) -> &World {
        &self.world
    }

    /// Enable (or disable) the opt-in exact pre-inference budget guard for this
    /// world - see `inference_bridge::InferenceJob::exact_token_counting`.
    /// Call once at startup when the run config requests it, before serving.
    pub fn set_exact_token_counting(&mut self, enabled: bool) {
        // `InferenceStage` is inserted by every `PipelineWorld::new` path, so it
        // is a hard invariant here - `resource_mut` (which panics if absent) is
        // correct and keeps this branch-free.
        self.world
            .resource_mut::<crate::pipeline::InferenceStage>()
            .exact_token_counting = enabled;
    }

    /// Install the shared interaction hub as a world resource and attach this
    /// world's wake handle to it, so opening/answering a prompt wakes the driver
    /// and [`reflect_interaction_status`]
    /// mirrors the change into agent status. Call once at startup, before
    /// serving. Without this, that system is a no-op (test worlds).
    pub fn insert_interaction_hub(&mut self, hub: crate::interaction_hub::InteractionHub) {
        hub.attach_wake(self.wake.clone());
        self.world.insert_resource(hub);
    }

    /// Spawn an agent from its pre-built component bundle and wake the driver so
    /// the next fixed-point picks it up. Returns the new entity.
    pub fn spawn_agent(&mut self, bundle: impl Bundle) -> Entity {
        let e = self.world.spawn(bundle).id();
        self.wake.notify_one();
        e
    }

    /// Spawn an agent from a blueprint + task + per-stage resolution (see
    /// [`crate::pipeline::spawn_agent`]) and wake the driver. Returns the new
    /// entity, or an error if the first stage's system prompt doesn't fit.
    pub fn spawn_from_blueprint(
        &mut self,
        agent_id: String,
        blueprint: leviath_core::Blueprint,
        task: &str,
        stages: Vec<crate::pipeline::ResolvedStage>,
        global_batch_tool_hint: bool,
    ) -> Result<Entity, String> {
        let e = crate::pipeline::spawn_agent(
            &mut self.world,
            agent_id,
            blueprint,
            task,
            stages,
            global_batch_tool_hint,
        )?;
        self.wake.notify_one();
        Ok(e)
    }

    /// Deliver a message to a running agent (routed to its inbox on the next
    /// tick) and wake the driver.
    pub fn send_message(&self, msg: AgentMessage) -> Result<(), ProviderError> {
        self.msg_tx
            .send(msg)
            .map_err(|e| ProviderError::Other(format!("world message channel closed: {e}")))?;
        self.wake.notify_one();
        Ok(())
    }

    /// A clone of the wake handle, so external producers (e.g. a control socket)
    /// can nudge the driver after mutating the world directly.
    pub fn wake_handle(&self) -> Arc<Notify> {
        self.wake.clone()
    }

    /// Request the [`Self::run`] loop to stop after its current fixed point.
    pub fn shutdown(&self) {
        self.shutdown.notify_one();
    }

    /// A clone of the shutdown handle, so a supervisor can stop a [`Self::run`]
    /// loop that has taken ownership of the world on another task.
    pub fn shutdown_handle(&self) -> Arc<Notify> {
        self.shutdown.clone()
    }

    /// Cleanly stop the world, guaranteeing every queued snapshot reaches disk.
    ///
    /// The persistence lane is async and fire-and-forget, so a plain shutdown (the
    /// [`Self::run`]/`serve` loop returning, then the world dropping) can lose
    /// snapshots still queued in the channel. This method closes that gap: it
    /// signals shutdown, drives one last fixed point so any state that settled
    /// after the loop parked is dispatched to the lane, then **closes the lane and
    /// awaits the worker** so all queued writes (`meta.json` / `context.json` /
    /// `run.lvr`) land before it returns.
    ///
    /// Call it after the serve loop has returned (the tokio runtime must still be
    /// alive for the worker to be scheduled). Idempotent: a second call is a no-op
    /// because the persistence resource is already removed and the task taken.
    pub async fn flush_and_stop(&mut self) {
        // Idempotent - the serve loop has usually already returned on this signal.
        self.shutdown.notify_one();
        // Dispatch anything that settled between the last park and now (e.g. an
        // inference result that woke the loop the same instant shutdown fired).
        self.run_to_fixed_point();
        // Drop the *only* `PersistJob` sender so the worker's `recv()` loop drains
        // its queue and then ends.
        self.world.remove_resource::<PersistenceStage>();
        // Wait for every queued write to hit disk.
        if let Some(task) = self.persist_task.take() {
            let _ = task.await;
        }
        // Push any buffered telemetry export out before the process goes away;
        // the final fixed point above already emitted the last events. The
        // resource always exists - `new()` installs the no-op default.
        self.world
            .resource::<crate::telemetry::Telemetry>()
            .0
            .force_flush();
    }

    /// The status of an agent, if it still exists.
    pub fn agent_status(&self, entity: Entity) -> Option<AgentStatus> {
        self.world
            .get::<AgentState>(entity)
            .map(|s| s.status.clone())
    }

    /// Set an agent's status and wake the driver. Returns `false` if the agent no
    /// longer exists. The async-starting dispatchers only act on `Active` agents,
    /// so this is how the world pauses/resumes/cancels an agent - a non-`Active`
    /// agent is simply data the systems skip until it is `Active` again.
    pub fn set_status(&mut self, entity: Entity, status: AgentStatus) -> bool {
        let Some(mut state) = self.world.get_mut::<AgentState>(entity) else {
            return false;
        };
        state.status = status;
        self.wake.notify_one();
        true
    }

    /// Pause an agent (it finishes any in-flight step, then stops before starting
    /// new work). Returns `false` if the agent no longer exists.
    pub fn pause(&mut self, entity: Entity) -> bool {
        self.set_status(entity, AgentStatus::Idle)
    }

    /// Resume a paused agent.
    pub fn resume(&mut self, entity: Entity) -> bool {
        self.set_status(entity, AgentStatus::Active)
    }

    /// Cancel an agent (it stops starting new work; in-flight results still land).
    pub fn cancel(&mut self, entity: Entity) -> bool {
        self.set_status(entity, AgentStatus::Cancelled)
    }

    /// Run one schedule tick over every agent, catching a panic from any system
    /// so one bad agent can't crash the daemon and take every other hosted agent
    /// with it.
    ///
    /// When the panic can be traced to a specific agent (the usual case - see
    /// `tick_scope`), that agent is failed with the panic message so it
    /// stops being driven, its run is persisted as errored, and the host reaps
    /// it. Without that, the world would re-tick the same unchanged state on
    /// every wake and panic again indefinitely.
    pub fn tick(&mut self) -> TickOutcome {
        let Err(panicked) = run_isolated(&mut self.schedule, &mut self.world) else {
            // A clean unwind doesn't mean a clean tick: work that ran on the
            // compute pool catches its own panics, since they can't unwind back
            // here, and leaves a marker instead.
            return self.fail_agents_panicked_in_parallel();
        };
        let message = panic_status_message(&panicked.message);
        match panicked.entity {
            Some(entity) if self.set_status(entity, AgentStatus::Error { message }) => {
                tracing::error!(
                    ?entity,
                    panic = %panicked.message,
                    "a pipeline system panicked; failing that agent - the daemon and every \
                     other run keep going"
                );
                TickOutcome::AgentFailed
            }
            _ => {
                tracing::error!(
                    panic = %panicked.message,
                    "a pipeline system panicked outside any agent's scope; the daemon survived \
                     (an agent may be wedged - cancel it via `lev cancel <run-id>`)"
                );
                TickOutcome::Unattributed
            }
        }
    }

    /// Fail every agent that a compute-pool body marked
    /// [`PanickedInParallel`](crate::tick_scope::PanickedInParallel), and report
    /// whether there were any.
    ///
    /// These panics were caught on a task-pool thread rather than unwinding into
    /// `tick`, so the marker component is how they reach the driver - but from
    /// here on they are handled exactly like an attributed unwind: the agent is
    /// failed, stops being driven, and its run persists as errored.
    fn fail_agents_panicked_in_parallel(&mut self) -> TickOutcome {
        let mut query = self
            .world
            .query::<(Entity, &crate::tick_scope::PanickedInParallel)>();
        let failed: Vec<(Entity, String)> = query
            .iter(&self.world)
            .map(|(entity, p)| (entity, p.message.clone()))
            .collect();
        if failed.is_empty() {
            return TickOutcome::Clean;
        }
        for (entity, message) in failed {
            self.world
                .entity_mut(entity)
                .remove::<crate::tick_scope::PanickedInParallel>();
            let status = AgentStatus::Error {
                message: panic_status_message(&message),
            };
            // The entity came straight out of the query above, so it exists.
            let _ = self.set_status(entity, status);
        }
        TickOutcome::AgentFailed
    }

    /// Append a system to the schedule (test-only, for panic-isolation tests).
    #[cfg(test)]
    pub(crate) fn add_test_system<M>(
        &mut self,
        // `IntoSystemConfigs` became `IntoScheduleConfigs<ScheduleSystem, _>` in
        // bevy_ecs 0.19 (it now also describes observer and other schedulables,
        // so the schedulable kind is an explicit parameter).
        system: impl bevy_ecs::schedule::IntoScheduleConfigs<bevy_ecs::system::ScheduleSystem, M>,
    ) {
        self.schedule.add_systems(system);
    }

    fn count<F: QueryFilter>(&mut self) -> usize {
        let mut q = self.world.query_filtered::<(), F>();
        q.iter(&self.world).count()
    }

    /// Snapshot the per-phase marker counts.
    fn fingerprint(&mut self) -> Fingerprint {
        [
            self.count::<With<ReadyToInfer>>(),
            self.count::<With<AwaitingInference>>(),
            self.count::<With<ProcessResponse>>(),
            self.count::<With<ReadyForTools>>(),
            self.count::<With<ReadyForTransition>>(),
            self.count::<With<ResolveTransition>>(),
            self.count::<With<AwaitingTools>>(),
            self.count::<With<AwaitingTransitionChoice>>(),
            self.count::<With<AwaitingTransitionResponse>>(),
            self.count::<With<AwaitingCompaction>>(),
            self.count::<With<crate::title::PendingTitle>>(),
            self.count::<With<crate::title::AwaitingTitle>>(),
        ]
    }

    /// Any agent waiting on an in-flight async job (inference, tools, a
    /// transition choice, or compaction) whose completion will wake the driver.
    fn has_async_inflight(&mut self) -> bool {
        self.count::<With<AwaitingInference>>() > 0
            || self.count::<With<AwaitingTools>>() > 0
            || self.count::<With<AwaitingTransitionResponse>>() > 0
            || self.count::<With<AwaitingCompaction>>() > 0
            || self.count::<With<crate::title::AwaitingTitle>>() > 0
    }

    /// Drive the schedule until a tick changes nothing (quiescence). Public so a
    /// host loop can interleave control operations between quiescent points.
    pub fn run_to_fixed_point(&mut self) {
        let mut prev = self.fingerprint();
        let mut failures = 0;
        loop {
            let outcome = self.tick();
            match outcome {
                TickOutcome::Clean => {}
                // The offending agent has been failed, so it won't be driven
                // again. Keep ticking: the rest of the world still has work to
                // do, and only a later tick reaches `dispatch_persistence` (the
                // last system in the chain) to record the failure on disk. The
                // budget stops a pathological agent that somehow panics again
                // from spinning this loop.
                TickOutcome::AgentFailed if failures < MAX_TICK_FAILURES_PER_ROUND => {
                    failures += 1;
                }
                // Nothing to fail, so re-ticking would just re-panic: stop
                // driving this round. The daemon stays alive, other agents keep
                // running, and a wedged agent can be cancelled via the control
                // socket (dispatch systems skip non-Active agents once
                // cancelled).
                TickOutcome::AgentFailed | TickOutcome::Unattributed => break,
            }
            let now = self.fingerprint();
            // Quiescence, but only trust it after a clean tick: a panicking tick
            // abandons the rest of the chain (and its buffered commands), so the
            // markers can look unchanged while the world very much has changed.
            // Force at least one more tick so the failed agent gets persisted.
            if now == prev && outcome == TickOutcome::Clean {
                break;
            }
            prev = now;
        }
    }

    /// Drive every agent as far as it can go **right now**, then, while async
    /// work is in flight, wait for each completion and drive again - returning
    /// once the world is fully quiescent with nothing in flight. Bounded by
    /// `max_waits` wake-waits as a safety valve so a lost/never-arriving wake
    /// can't hang a caller (e.g. a test) forever.
    pub async fn run_until_idle(&mut self, max_waits: usize) {
        self.run_to_fixed_point();
        let mut waits = 0;
        while self.has_async_inflight() && waits < max_waits {
            self.wake.notified().await;
            waits += 1;
            self.run_to_fixed_point();
        }
    }

    /// Run forever: drive to quiescence, then park until an async completion or
    /// an external `send_message`/`spawn_agent` wakes the driver. Returns when
    /// [`Self::shutdown`] is signalled.
    pub async fn run(&mut self) {
        loop {
            self.run_to_fixed_point();
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = self.shutdown.notified() => return,
            }
        }
    }
}

/// How a caught panic is recorded on the agent it is blamed on. Shared by the
/// unwind path and the compute-pool path so a run's `error` reads the same
/// either way.
fn panic_status_message(panic: &str) -> String {
    format!("internal error: a pipeline system panicked: {panic}")
}

/// A panic caught while ticking the schedule, and the agent it belongs to.
struct TickPanic {
    /// The agent being processed when the panic fired, if the pipeline had
    /// recorded one (see [`crate::tick_scope`]).
    entity: Option<Entity>,
    /// The panic payload rendered as text.
    message: String,
}

/// Run a schedule over a world, catching a panic from any system so it can't
/// unwind the daemon's drive loop and take down every hosted agent.
///
/// The world may be partially updated after a panic: the panicking system's
/// buffered `Commands` are lost, but resources and components already written
/// are intact, so the caller can still fail the offending agent.
fn run_isolated(schedule: &mut Schedule, world: &mut World) -> Result<(), TickPanic> {
    // Clear first: the slot is thread-local and survives across ticks, so a
    // stale entity from an earlier tick must not be blamed for this one.
    crate::tick_scope::clear();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| schedule.run(world))) {
        Ok(()) => Ok(()),
        Err(payload) => {
            reset_executor(schedule);
            Err(TickPanic {
                entity: crate::tick_scope::current(),
                message: leviath_core::panic_message(payload.as_ref()),
            })
        }
    }
}

/// Give `schedule` a fresh executor after a caught panic.
///
/// bevy's executors mark a system "completed" *before* running it and only
/// clear that set when `run` returns normally. A panic therefore leaves every
/// system up to and including the offending one marked done, so the **next**
/// tick silently skips them and only runs the tail of the chain - a partial
/// tick that would, among other things, keep `dispatch_persistence` from ever
/// seeing an agent we just failed. Swapping the executor kind and back is the
/// public API for forcing a rebuild.
///
/// One call suffices on bevy_ecs 0.19: `set_executor` takes an executor
/// *instance* and unconditionally replaces `schedule.executor` with it (clearing
/// `executor_initialized` too), so the fresh `SingleThreadedExecutor` arrives
/// with an empty `completed_systems`.
///
/// On 0.15 this had to set two different *kinds* and swap back, because
/// `set_executor_kind` was a no-op when the kind was unchanged - and
/// `SimpleExecutor`, the other kind it used, no longer exists.
fn reset_executor(schedule: &mut Schedule) {
    schedule.set_executor(bevy_ecs::schedule::SingleThreadedExecutor::new());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes every test in this binary that swaps the **process-global**
    /// panic hook - see the definition for why they can't run concurrently.
    use crate::test_support::PANIC_HOOK_LOCK;

    /// Run `f` with the process panic hook silenced (the panic is expected), and
    /// serialized against the other hook-swapping tests.
    fn with_silent_panics<T>(f: impl FnOnce() -> T) -> T {
        let _hook_guard = PANIC_HOOK_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let out = f();
        std::panic::set_hook(prev_hook);
        out
    }

    #[test]
    fn run_isolated_catches_a_system_panic_and_reports_the_agent() {
        fn ok_system() {}
        fn boom_system() {
            panic!("simulated system panic");
        }
        // A system that panics *while working on a specific agent* - the shape
        // every real pipeline system has.
        fn boom_on_agent_system() {
            crate::tick_scope::enter(
                Entity::from_raw_u32(41)
                    .expect("a small literal index is always a valid entity id"),
            );
            panic!("agent-scoped panic");
        }
        let mut world = World::new();

        // A clean schedule ticks normally.
        let mut ok = tick_schedule();
        ok.add_systems(ok_system);
        assert!(run_isolated(&mut ok, &mut world).is_ok());

        // A panicking system is caught (the daemon would survive) and, with no
        // agent in scope, reports no entity to blame.
        let mut bad = tick_schedule();
        bad.add_systems(boom_system);
        let err = with_silent_panics(|| run_isolated(&mut bad, &mut world))
            .expect_err("the panic must be caught");
        assert_eq!(err.entity, None);
        assert_eq!(err.message, "simulated system panic");

        // With an agent in scope, the panic is attributed to it.
        let mut blamed = tick_schedule();
        blamed.add_systems(boom_on_agent_system);
        let err = with_silent_panics(|| run_isolated(&mut blamed, &mut world))
            .expect_err("the panic must be caught");
        assert_eq!(
            err.entity,
            Some(
                Entity::from_raw_u32(41)
                    .expect("a small literal index is always a valid entity id")
            )
        );
        assert_eq!(err.message, "agent-scoped panic");

        // A later clean tick must not inherit the previous tick's entity.
        assert!(run_isolated(&mut ok, &mut world).is_ok());
        assert_eq!(crate::tick_scope::current(), None);
    }

    use crate::components::{AgentState, ContextWindow, InferenceConfig};
    use crate::pipeline::{
        AgentBlueprint, MessageIntake, StageCursor, StageInference, StageInferences, StageProgress,
        StageSetup, StageSetups, VisitCounts,
    };
    use crate::tool_bridge::BoxedToolExec;
    use leviath_core::{Region, RegionKind};
    use leviath_providers::{
        FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, Provider, TokenUsage,
        ToolCall,
    };
    use std::sync::Mutex;

    /// A provider scripted with a queue of responses; each `infer` pops the next.
    struct Script {
        responses: Mutex<std::collections::VecDeque<InferenceResponse>>,
    }

    #[async_trait::async_trait]
    impl Provider for Script {
        async fn infer(
            &self,
            _req: InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            let next = self.responses.lock().unwrap().pop_front();
            next.ok_or_else(|| ProviderError::Other("script exhausted".to_string()))
        }
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "script"
        }
        fn capabilities(&self, _m: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }

    fn text(content: &str) -> InferenceResponse {
        InferenceResponse {
            content: content.to_string(),
            tool_calls: vec![],
            tokens_used: TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            finish_reason: FinishReason::Complete,
        }
    }

    fn with_tool(id: &str, name: &str) -> InferenceResponse {
        let mut r = text("");
        r.tool_calls.push(ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({}),
            thought_signature: None,
        });
        r
    }

    /// A tool service that returns a fixed result string for every call.
    struct EchoTools;
    impl ToolService for EchoTools {
        fn exec_for(&self, _entity: Entity, calls: Vec<ToolCall>) -> BoxedToolExec {
            Box::new(move || {
                Box::pin(async move {
                    calls
                        .into_iter()
                        .map(|c| (c.id, "ok".to_string()))
                        .collect()
                })
            })
        }
    }

    fn window() -> ContextWindow {
        let mut w = ContextWindow::new(10_000);
        w.add_region(Region::new("sys".to_string(), RegionKind::Pinned, 2000));
        w.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::Clearable,
            10_000,
        ));
        w.add_region(Region::new(
            "tool_results".to_string(),
            RegionKind::Temporary,
            5000,
        ));
        w
    }

    fn agent_state() -> AgentState {
        AgentState {
            agent_id: "a".to_string(),
            current_stage: "s".to_string(),
            iteration: 0,
            status: AgentStatus::Active,
            spawned_children_ids: vec![],
            pending_wait: None,
            accepts_messages: true,
        }
    }

    /// A stage advertising the tools the scripted responses here actually call.
    ///
    /// Advertising them is load-bearing: dispatch refuses tools a stage never
    /// offered, so with an empty tool list every end-to-end test that drives a
    /// tool call would short-circuit into a refusal and the tool service would
    /// never be reached at all.
    fn stage(model: &str) -> StageInference {
        StageInference {
            provider_name: "script".to_string(),
            model: model.to_string(),
            tools: ["do", "read"]
                .iter()
                .map(|n| leviath_providers::Tool {
                    name: (*n).to_string(),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                })
                .collect(),
            tool_filter: None,
        }
    }

    fn setup() -> StageSetup {
        StageSetup {
            inference_config: InferenceConfig {
                temperature: None,
                max_output_tokens: None,
                extra_params: Default::default(),
                batch_tool_hint: false,
                request_timeout_secs: None,
            },
            routing: None,
            accepts_messages: true,
            context_layout: None,
            system_prompt: None,
        }
    }

    fn blueprint() -> leviath_core::Blueprint {
        let layout = leviath_core::layout::ContextLayout::new(
            vec![leviath_core::layout::RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::Clearable,
                10_000,
            )],
            12_000,
        );
        let s = leviath_core::Stage::new(
            "s".to_string(),
            leviath_core::blueprint::ModelConfig::new("script".to_string(), "m".to_string()),
        );
        leviath_core::Blueprint::new("t".to_string(), "d".to_string(), vec![s], layout)
    }

    /// Spawn a single-stage agent, initially ready to infer.
    fn spawn(world: &mut PipelineWorld) -> Entity {
        world.spawn_agent((
            AgentBlueprint(blueprint()),
            StageCursor { index: 0 },
            agent_state(),
            crate::components::MessageInbox::default(),
            StageProgress::default(),
            StageInferences(vec![stage("m")]),
            StageSetups(vec![setup()]),
            VisitCounts::default(),
            window(),
            stage("m"),
            setup().inference_config,
            ReadyToInfer,
        ))
    }

    fn build_world(providers: ProviderRegistry) -> PipelineWorld {
        // These agents carry no RunMetadata, so persistence never fires and the
        // runs dir is never written; any path is fine.
        PipelineWorld::new(
            providers,
            Arc::new(EchoTools),
            InferencePoolConfig::new(),
            1,
            std::env::temp_dir(),
            Handle::current(),
        )
    }

    #[tokio::test]
    async fn set_exact_token_counting_toggles_the_stage_flag() {
        let mut world = build_world(ProviderRegistry::new());
        // Default is off.
        assert!(
            !world
                .world()
                .resource::<crate::pipeline::InferenceStage>()
                .exact_token_counting
        );
        world.set_exact_token_counting(true);
        assert!(
            world
                .world()
                .resource::<crate::pipeline::InferenceStage>()
                .exact_token_counting
        );
    }

    #[tokio::test]
    async fn run_to_fixed_point_survives_a_panicking_system() {
        // A system that panics must not hang or crash the drive loop - it's
        // caught and the loop breaks (the daemon survives).
        fn boom_system() {
            panic!("simulated system panic");
        }
        let mut world = build_world(ProviderRegistry::new());
        world.add_test_system(boom_system);
        // Unattributed: nothing to fail, so the round stops immediately.
        with_silent_panics(|| world.run_to_fixed_point());
    }

    #[tokio::test]
    async fn a_panic_on_the_compute_pool_is_attributed_to_its_agent() {
        // `dispatch_inference` fans its per-agent work out over the compute task
        // pool, where the thread-local scope can't reach the driver thread that
        // catches unwinds. Those bodies run under `run_agent_parallel`, which
        // catches on the pool thread and marks the agent instead - this proves
        // the marker makes it back and fails the right run (issue #109).
        fn boom_in_parallel(
            agents: Query<(Entity, &AgentState)>,
            par_commands: bevy_ecs::system::ParallelCommands,
        ) {
            agents.par_iter().for_each(|(entity, state)| {
                if state.status != AgentStatus::Active {
                    return; // already failed - nothing left to blow up
                }
                // Clear the thread-local first: whatever attributes this panic,
                // it is demonstrably not the `enter`/`current` mechanism.
                crate::tick_scope::clear();
                crate::tick_scope::run_agent_parallel(entity, &par_commands, &mut || {
                    panic!("blew up on the compute pool");
                });
            });
        }

        let mut world = build_world(ProviderRegistry::new());
        let entity = spawn(&mut world);
        world.add_test_system(boom_in_parallel);
        with_silent_panics(|| world.run_to_fixed_point());

        let status = world.agent_status(entity);
        assert!(
            matches!(status, Some(AgentStatus::Error { ref message })
                if message.contains("a pipeline system panicked")
                    && message.contains("blew up on the compute pool")),
            "got: {status:?}"
        );
        // The marker is consumed, so a later tick doesn't re-fail the agent.
        assert!(
            world
                .world()
                .entity(entity)
                .get::<crate::tick_scope::PanickedInParallel>()
                .is_none(),
            "the marker must be drained once acted on"
        );
    }

    #[tokio::test]
    async fn a_panicking_system_fails_its_agent_instead_of_looping_forever() {
        // Before issue #109 was fixed, a panicking system was swallowed
        // anonymously: nothing changed, so the very next wake re-ticked the same
        // state and panicked again, forever, while every other agent stalled.
        // Now the agent in scope is failed, which takes it out of the dispatch
        // systems (they only act on `Active` agents) and lets the world settle.
        static VICTIM: std::sync::Mutex<Option<Entity>> = std::sync::Mutex::new(None);
        static PANICS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

        fn boom_on_active_agent(agents: Query<(Entity, &AgentState)>) {
            // No trailing statements after the `panic!`: an unreachable tail
            // would read as uncovered under the workspace's 100% gate.
            let Some((entity, _)) = agents
                .iter()
                .find(|(_, state)| state.status == AgentStatus::Active)
            else {
                return; // the agent has been failed - nothing left to blow up
            };
            crate::tick_scope::enter(entity);
            *VICTIM
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(entity);
            PANICS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("blew up on this agent");
        }

        let mut world = build_world(ProviderRegistry::new());
        let entity = spawn(&mut world);
        world.add_test_system(boom_on_active_agent);
        with_silent_panics(|| world.run_to_fixed_point());

        let victim = VICTIM
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        assert_eq!(victim, Some(entity), "the system saw the spawned agent");
        let status = world.agent_status(entity);
        assert!(
            matches!(status, Some(AgentStatus::Error { ref message })
                if message.contains("a pipeline system panicked")
                    && message.contains("blew up on this agent")),
            "got: {status:?}"
        );
        // The loop terminated rather than re-panicking without bound.
        assert!(
            PANICS.load(std::sync::atomic::Ordering::SeqCst) <= MAX_TICK_FAILURES_PER_ROUND + 1,
            "the panic budget must stop the round"
        );
    }

    fn registry_with(responses: Vec<InferenceResponse>) -> ProviderRegistry {
        let mut r = ProviderRegistry::new();
        r.register(
            "script".to_string(),
            Arc::new(Script {
                responses: Mutex::new(responses.into_iter().collect()),
            }),
        );
        r
    }

    #[tokio::test]
    async fn agent_completes_after_nudges_exhausted() {
        // Text-only responses with no tool calls get nudged up to the max; the
        // response after the last nudge is accepted and the single-stage
        // blueprint terminates the agent. (Exercises the handle_empty_response
        // nudge loop end-to-end through the driver.)
        let mut world = build_world(registry_with(vec![
            text("thinking"),
            text("still"),
            text("more"),
            text("final"),
        ]));
        let e = spawn(&mut world);

        world.run_until_idle(30).await;

        assert_eq!(world.agent_status(e), Some(AgentStatus::Complete));
    }

    #[tokio::test]
    async fn agent_runs_tools_then_completes() {
        // First response calls a tool; after the tool result comes back the
        // second response is text-only, finishing the run.
        let mut world = build_world(registry_with(vec![with_tool("c1", "do"), text("done")]));
        let e = spawn(&mut world);

        world.run_until_idle(20).await;

        assert_eq!(world.agent_status(e), Some(AgentStatus::Complete));
        // With no routing configured, tool results land in the conversation
        // region.
        assert!(
            world
                .world()
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .current_tokens
                > 0
        );
    }

    #[tokio::test]
    async fn insert_interaction_hub_installs_resource_and_attaches_wake() {
        use crate::dynamic_interaction::InteractionBackend;
        use crate::interaction_hub::InteractionHub;
        let mut world = build_world(registry_with(vec![]));
        let hub = InteractionHub::new();
        world.insert_interaction_hub(hub.clone());

        // The hub is now a world resource the reflect system reads.
        assert!(world.world().get_resource::<InteractionHub>().is_some());

        // The wake handle was attached: opening a request nudges the same wake
        // the driver parks on (a later notified() returns immediately).
        let backend = hub.backend_for("x");
        let asking = tokio::spawn(async move {
            backend
                .ask(leviath_core::interaction::InteractionRequest::free_text(
                    "q", "p", "s", true,
                ))
                .await
        });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        world.wake_handle().notified().await;
        hub.cancel("q");
        let _ = asking.await;
    }

    #[tokio::test]
    async fn provider_error_marks_agent_error() {
        // Empty script ⇒ the very first infer errors.
        let mut world = build_world(registry_with(vec![]));
        let e = spawn(&mut world);

        world.run_until_idle(20).await;

        assert_eq!(
            std::mem::discriminant(&world.agent_status(e).unwrap()),
            std::mem::discriminant(&AgentStatus::Error {
                message: String::new()
            })
        );
    }

    #[tokio::test]
    async fn send_message_reaches_the_agent_inbox() {
        // No responses queued: the agent dispatches inference and parks awaiting
        // it. We deliver a message; the deliver system routes it to context.
        let mut world = build_world(registry_with(vec![]));
        let e = spawn(&mut world);
        // Drive to the point the first (doomed) inference is dispatched/collected.
        world.run_until_idle(20).await;

        world
            .send_message(AgentMessage {
                agent_id: "a".to_string(),
                content: "hello".to_string(),
                target_region: Some("conversation".to_string()),
                priority: 0,
            })
            .unwrap();
        world.tick(); // deliver_messages runs

        assert!(
            world
                .world()
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .current_tokens
                > 0
        );
    }

    #[tokio::test]
    async fn run_returns_on_shutdown() {
        let mut world = build_world(registry_with(vec![text("done")]));
        spawn(&mut world);
        world.shutdown(); // pre-signal: run parks then returns
        // Must return rather than loop forever.
        world.run().await;
    }

    #[tokio::test]
    async fn run_wakes_then_shuts_down() {
        // Drives run() on its own task: a wake makes it loop once (wake branch),
        // then a shutdown makes it return (shutdown branch).
        let mut world = build_world(registry_with(vec![
            text("t1"),
            text("t2"),
            text("t3"),
            text("t4"),
        ]));
        spawn(&mut world);
        let wake = world.wake_handle();
        let shutdown = world.shutdown_handle();
        let handle = tokio::spawn(async move { world.run().await });

        wake.notify_one();
        tokio::task::yield_now().await;
        shutdown.notify_one();

        handle.await.unwrap(); // returns cleanly
    }

    #[tokio::test]
    async fn send_message_errors_when_intake_dropped() {
        let mut world = build_world(registry_with(vec![]));
        // Drop the intake receiver via the world accessor, closing the channel.
        let removed = world.world_mut().remove_resource::<MessageIntake>();
        drop(removed);

        let err = world.send_message(AgentMessage {
            agent_id: "a".to_string(),
            content: "x".to_string(),
            target_region: None,
            priority: 0,
        });
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn script_provider_metadata_is_exercised() {
        // Keep the mock's non-`infer`/`capabilities` methods measured.
        let p = Script {
            responses: Mutex::new(std::collections::VecDeque::new()),
        };
        assert_eq!(p.name(), "script");
        assert_eq!(p.count_tokens("t", "m").await, 1);
        assert_eq!(p.max_context_tokens("m"), 100_000);
        let _ = p.capabilities("m");
    }

    #[tokio::test]
    async fn agent_status_is_none_for_unknown_entity() {
        let world = build_world(registry_with(vec![]));
        assert_eq!(
            world.agent_status(
                Entity::from_raw_u32(999)
                    .expect("a small literal index is always a valid entity id")
            ),
            None
        );
    }

    #[tokio::test]
    async fn paused_agent_does_not_progress_until_resumed() {
        let mut world = build_world(registry_with(vec![
            text("t1"),
            text("t2"),
            text("t3"),
            text("t4"),
        ]));
        let e = spawn(&mut world);
        assert!(world.pause(e));

        world.run_until_idle(30).await;
        // Paused ⇒ parked as Idle, never inferred.
        assert_eq!(world.agent_status(e), Some(AgentStatus::Idle));

        assert!(world.resume(e));
        world.run_until_idle(30).await;
        assert_eq!(world.agent_status(e), Some(AgentStatus::Complete));
    }

    #[tokio::test]
    async fn cancelled_agent_stops_progressing() {
        let mut world = build_world(registry_with(vec![with_tool("c1", "do"), text("done")]));
        let e = spawn(&mut world);
        assert!(world.cancel(e));

        world.run_until_idle(20).await;

        assert_eq!(world.agent_status(e), Some(AgentStatus::Cancelled));
    }

    #[tokio::test]
    async fn status_ops_return_false_for_unknown_entity() {
        let mut world = build_world(registry_with(vec![]));
        assert!(!world.pause(
            Entity::from_raw_u32(999).expect("a small literal index is always a valid entity id")
        ));
        assert!(!world.resume(
            Entity::from_raw_u32(999).expect("a small literal index is always a valid entity id")
        ));
        assert!(!world.cancel(
            Entity::from_raw_u32(999).expect("a small literal index is always a valid entity id")
        ));
    }

    #[tokio::test]
    async fn spawn_from_blueprint_builds_a_runnable_agent() {
        // End-to-end via the blueprint resolver: build → drive → complete.
        let mut world = build_world(registry_with(vec![with_tool("c1", "do"), text("done")]));
        let e = world
            .spawn_from_blueprint(
                "agent-1".to_string(),
                blueprint(),
                "do the task",
                vec![crate::pipeline::ResolvedStage {
                    provider_name: "script".to_string(),
                    model: "m".to_string(),
                    tools: vec![],
                }],
                true,
            )
            .unwrap();

        world.run_until_idle(20).await;

        assert_eq!(world.agent_status(e), Some(AgentStatus::Complete));
    }

    #[tokio::test]
    async fn persists_agent_snapshot_to_runs_dir() {
        // An agent carrying RunMetadata + TokenTotals is snapshotted to disk as it
        // runs; after it completes, meta.json exists with the final status.
        let dir = tempfile::tempdir().unwrap();
        let mut world = PipelineWorld::new(
            registry_with(vec![with_tool("c1", "do"), text("done")]),
            Arc::new(EchoTools),
            InferencePoolConfig::new(),
            1,
            dir.path().to_path_buf(),
            Handle::current(),
        );
        world.spawn_agent((
            AgentBlueprint(blueprint()),
            StageCursor { index: 0 },
            agent_state(),
            crate::components::MessageInbox::default(),
            StageProgress::default(),
            StageInferences(vec![stage("m")]),
            StageSetups(vec![setup()]),
            VisitCounts::default(),
            window(),
            stage("m"),
            setup().inference_config,
            crate::persistence::RunMetadata {
                run_id: "run-42".to_string(),
                agent_name: "a".to_string(),
                agent_path: "/p".to_string(),
                task: "t".to_string(),
                model: None,
                // A real directory: the tick chain fails a run whose workspace is gone.
                workdir: std::env::temp_dir().to_string_lossy().to_string(),
                num_stages: 1,
                started_at: 0,
                parent_run_id: None,
                metadata: std::collections::HashMap::new(),
                callback_url: None,
                callback_secret: None,
                title: None,
            },
            crate::persistence::TokenTotals::default(),
            crate::pipeline::PersistWatermark::default(),
            ReadyToInfer,
        ));

        world.run_until_idle(20).await;

        // The persistence worker is fire-and-forget on its own task; poll until the
        // final (Complete) snapshot has been flushed. A short real sleep between
        // polls (rather than a bare `yield_now`) gives the worker's write actual
        // wall-clock time to land under load - otherwise the loop can spin through
        // every iteration before the write completes and spuriously time out.
        let meta_path = dir.path().join("run-42").join("meta.json");
        let mut meta = None;
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(&meta_path)
                && let Ok(m) = serde_json::from_str::<leviath_core::run_meta::RunMeta>(&text)
                && m.status == leviath_core::run_meta::RunStatus::Complete
            {
                meta = Some(m);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let meta = meta.expect("final Complete snapshot flushed to disk");
        assert_eq!(meta.run_id, "run-42");
        assert!(dir.path().join("run-42").join("context.json").exists());
    }

    #[tokio::test]
    async fn a_panicked_agent_is_recorded_as_errored_on_disk() {
        // The reported symptom in issue #109: a crashed run stayed `"running"`
        // in meta.json forever. `dispatch_persistence` is the *last* system in
        // the chain, so the tick that panics never reaches it - which is exactly
        // why `run_to_fixed_point` keeps driving after failing the agent.
        fn boom_on_active_agent(agents: Query<(Entity, &AgentState)>) {
            let Some((entity, _)) = agents
                .iter()
                .find(|(_, state)| state.status == AgentStatus::Active)
            else {
                return; // the agent has been failed - nothing left to blow up
            };
            crate::tick_scope::enter(entity);
            panic!("exploded mid-stage");
        }

        let dir = tempfile::tempdir().unwrap();
        let mut world = PipelineWorld::new(
            registry_with(vec![]),
            Arc::new(EchoTools),
            InferencePoolConfig::new(),
            1,
            dir.path().to_path_buf(),
            Handle::current(),
        );
        world.spawn_agent((
            AgentBlueprint(blueprint()),
            StageCursor { index: 0 },
            agent_state(),
            crate::components::MessageInbox::default(),
            StageProgress::default(),
            StageInferences(vec![stage("m")]),
            StageSetups(vec![setup()]),
            VisitCounts::default(),
            window(),
            stage("m"),
            setup().inference_config,
            crate::persistence::RunMetadata {
                run_id: "run-boom".to_string(),
                agent_name: "a".to_string(),
                agent_path: "/p".to_string(),
                task: "t".to_string(),
                model: None,
                workdir: "/w".to_string(),
                num_stages: 1,
                started_at: 0,
                parent_run_id: None,
                metadata: std::collections::HashMap::new(),
                callback_url: None,
                callback_secret: None,
                title: None,
            },
            crate::persistence::TokenTotals::default(),
            crate::pipeline::PersistWatermark::default(),
            ReadyToInfer,
        ));
        world.add_test_system(boom_on_active_agent);
        with_silent_panics(|| world.run_to_fixed_point());

        let meta_path = dir.path().join("run-boom").join("meta.json");
        let mut meta = None;
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(&meta_path)
                && let Ok(m) = serde_json::from_str::<leviath_core::run_meta::RunMeta>(&text)
                && m.status == leviath_core::run_meta::RunStatus::Error
            {
                meta = Some(m);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let meta = meta.expect("the panicked run must be persisted as errored");
        let error = meta.error.unwrap_or_default();
        assert!(error.contains("a pipeline system panicked"), "got: {error}");
        assert!(error.contains("exploded mid-stage"), "got: {error}");
    }

    /// A single-stage blueprint whose stage is an `interactive_points` stage with a
    /// `plan_approval` point (the shape that blocks awaiting human approval).
    fn interactive_blueprint() -> leviath_core::Blueprint {
        use leviath_core::blueprint::{InteractionPoint, InteractionStyle, StageMode};
        let layout = leviath_core::layout::ContextLayout::new(
            vec![leviath_core::layout::RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::Clearable,
                10_000,
            )],
            12_000,
        );
        let mut s = leviath_core::Stage::new(
            "plan".to_string(),
            leviath_core::blueprint::ModelConfig::new("script".to_string(), "m".to_string()),
        );
        s.mode = StageMode::InteractivePoints {
            points: vec![InteractionPoint {
                name: "plan_approval".to_string(),
                prompt: "Approve?".to_string(),
                required: true,
                style: InteractionStyle::MultipleChoice,
                options: vec!["Approve".to_string(), "Abort".to_string()],
                directives: std::collections::HashMap::new(),
                abort_options: vec!["Abort".to_string()],
                edit_options: vec![],
                document_region: None,
            }],
        };
        leviath_core::Blueprint::new("t".to_string(), "d".to_string(), vec![s], layout)
    }

    #[tokio::test]
    async fn persists_interaction_point_when_a_live_agent_blocks() {
        // Drive a real agent through inference → transition → the interaction-point
        // lane until it blocks awaiting approval, and assert the daemon wrote the
        // `interactions.json` sidecar - the issue #38 persist side, end-to-end
        // through the live lane (a tool call first, then a text "plan", so the stage
        // transitions into the interaction point rather than looping on nudges).
        let dir = tempfile::tempdir().unwrap();
        let mut world = PipelineWorld::new(
            registry_with(vec![with_tool("c1", "read"), text("## Plan\n1. do it")]),
            Arc::new(EchoTools),
            InferencePoolConfig::new(),
            1,
            dir.path().to_path_buf(),
            Handle::current(),
        );
        world.insert_interaction_hub(crate::interaction_hub::InteractionHub::new());
        let e = world.spawn_agent((
            AgentBlueprint(interactive_blueprint()),
            StageCursor { index: 0 },
            agent_state(),
            crate::components::MessageInbox::default(),
            StageProgress::default(),
            StageInferences(vec![stage("m")]),
            StageSetups(vec![setup()]),
            VisitCounts::default(),
            window(),
            stage("m"),
            setup().inference_config,
            crate::persistence::RunMetadata {
                run_id: "run-ip".to_string(),
                agent_name: "a".to_string(),
                agent_path: "/p".to_string(),
                task: "t".to_string(),
                model: None,
                // A real directory: the tick chain fails a run whose workspace is gone.
                workdir: std::env::temp_dir().to_string_lossy().to_string(),
                num_stages: 1,
                started_at: 0,
                parent_run_id: None,
                metadata: std::collections::HashMap::new(),
                callback_url: None,
                callback_secret: None,
                title: None,
            },
            crate::persistence::TokenTotals::default(),
            crate::pipeline::PersistWatermark::default(),
            ReadyToInfer,
        ));

        world.run_until_idle(30).await;
        // `run_until_idle` stops once no inference/tool is in flight, but the
        // interaction-point ask task registers in the hub just after; the real
        // daemon's `run()` loop catches its wake, so pump fixed points here until
        // `reflect_interaction_status` flips the agent to Waiting (and persistence
        // captures the sidecar).
        for _ in 0..50 {
            if world.agent_status(e) == Some(AgentStatus::Waiting) {
                break;
            }
            tokio::task::yield_now().await;
            world.run_to_fixed_point();
        }
        assert_eq!(world.agent_status(e), Some(AgentStatus::Waiting));

        // Poll until the interaction sidecar lands (the persistence worker writes it
        // on its own task once the agent is parked Waiting at the point).
        let path = dir.path().join("run-ip").join("interactions.json");
        let mut sidecar = None;
        for _ in 0..200 {
            if let Ok(t) = std::fs::read_to_string(&path)
                && let Ok(s) =
                    serde_json::from_str::<crate::interaction_points::InteractionPointState>(&t)
            {
                sidecar = Some(s);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let s = sidecar.expect("interaction-point sidecar flushed to disk");
        assert_eq!(s.cursor, 0);
        assert_eq!(s.round, 0);
        assert_eq!(s.body, "## Plan\n1. do it");
    }

    #[tokio::test]
    async fn flush_and_stop_drains_queued_snapshots() {
        // Unlike a plain shutdown, `flush_and_stop` awaits the persistence worker,
        // so the final snapshot is guaranteed on disk the instant it returns - no
        // filesystem polling required (contrast the test above).
        let dir = tempfile::tempdir().unwrap();
        let mut world = PipelineWorld::new(
            registry_with(vec![with_tool("c1", "do"), text("done")]),
            Arc::new(EchoTools),
            InferencePoolConfig::new(),
            1,
            dir.path().to_path_buf(),
            Handle::current(),
        );
        world.spawn_agent((
            AgentBlueprint(blueprint()),
            StageCursor { index: 0 },
            agent_state(),
            crate::components::MessageInbox::default(),
            StageProgress::default(),
            StageInferences(vec![stage("m")]),
            StageSetups(vec![setup()]),
            VisitCounts::default(),
            window(),
            stage("m"),
            setup().inference_config,
            crate::persistence::RunMetadata {
                run_id: "run-flush".to_string(),
                agent_name: "a".to_string(),
                agent_path: "/p".to_string(),
                task: "t".to_string(),
                model: None,
                // A real directory: the tick chain fails a run whose workspace is gone.
                workdir: std::env::temp_dir().to_string_lossy().to_string(),
                num_stages: 1,
                started_at: 0,
                parent_run_id: None,
                metadata: std::collections::HashMap::new(),
                callback_url: None,
                callback_secret: None,
                title: None,
            },
            crate::persistence::TokenTotals::default(),
            crate::pipeline::PersistWatermark::default(),
            ReadyToInfer,
        ));

        world.run_until_idle(20).await;
        world.flush_and_stop().await;

        // Read immediately - the drain guarantees the write landed.
        let meta_path = dir.path().join("run-flush").join("meta.json");
        let text = std::fs::read_to_string(&meta_path).expect("meta.json flushed on stop");
        let meta: leviath_core::run_meta::RunMeta = serde_json::from_str(&text).unwrap();
        assert_eq!(meta.run_id, "run-flush");
        assert_eq!(meta.status, leviath_core::run_meta::RunStatus::Complete);

        // A second call is a no-op (resource already removed, task taken) - no panic.
        world.flush_and_stop().await;
        assert!(meta_path.exists());
    }

    #[tokio::test]
    async fn world_init_and_restore_needs_no_daemon_infra() {
        // `PipelineWorld::new` + `restore::restore_agent` form a self-contained
        // spin-up→restore path: no control socket, HTTP server, PID files, or build
        // markers - only providers, a tool service, a runs dir, and a runtime. This
        // locks that in so the daemon wiring stays optional.
        use leviath_core::region::EntryKind;
        use leviath_core::run_meta::{ContextSnapshot, RegionEntrySnapshot, RegionSnapshot};

        let dir = tempfile::tempdir().unwrap();
        let mut world = PipelineWorld::new(
            registry_with(vec![text("unused")]),
            Arc::new(EchoTools),
            InferencePoolConfig::new(),
            1,
            dir.path().to_path_buf(),
            Handle::current(),
        );
        let entity = world.spawn_agent((
            AgentBlueprint(blueprint()),
            StageCursor { index: 0 },
            agent_state(),
            crate::components::MessageInbox::default(),
            StageProgress::default(),
            StageInferences(vec![stage("m")]),
            StageSetups(vec![setup()]),
            VisitCounts::default(),
            window(),
            stage("m"),
            setup().inference_config,
            crate::persistence::TokenTotals::default(),
        ));

        let snapshot = ContextSnapshot {
            stage_name: "s0".to_string(),
            total_tokens: 4,
            max_tokens: 10_000,
            regions: vec![RegionSnapshot {
                name: "conversation".to_string(),
                kind: "clearable".to_string(),
                current_tokens: 4,
                max_tokens: 10_000,
                entries: vec![RegionEntrySnapshot {
                    content: "restored turn".to_string(),
                    tokens: 4,
                    kind: EntryKind::UserMessage,
                    metadata: None,
                    key: None,
                    taint: Default::default(),
                }],
            }],
        };
        crate::restore::restore_agent(
            world.world_mut(),
            entity,
            &snapshot,
            0,
            3,
            crate::persistence::TokenTotals::default(),
        );

        let state = world
            .world()
            .get::<crate::components::AgentState>(entity)
            .unwrap();
        assert_eq!(state.status, AgentStatus::Active);
        assert_eq!(state.iteration, 3);
        let win = world
            .world()
            .get::<crate::components::ContextWindow>(entity)
            .unwrap();
        assert_eq!(
            win.get_region("conversation").unwrap().content[0].content(),
            "restored turn"
        );
    }

    #[tokio::test]
    async fn spawn_from_blueprint_errors_on_oversized_system_prompt() {
        let mut world = build_world(registry_with(vec![]));
        // A blueprint whose stage carries an enormous system prompt in a tiny
        // pinned region overflows at spawn.
        let layout = leviath_core::layout::ContextLayout::new(
            vec![leviath_core::layout::RegionDefinition::new(
                "task".to_string(),
                RegionKind::Pinned,
                50,
            )],
            1000,
        );
        let mut s = leviath_core::Stage::new(
            "s".to_string(),
            leviath_core::blueprint::ModelConfig::new("script".to_string(), "m".to_string()),
        );
        s.config.insert(
            "system_prompt".to_string(),
            serde_json::Value::String("x".repeat(100_000)),
        );
        let bp = leviath_core::Blueprint::new("t".to_string(), "d".to_string(), vec![s], layout);

        let err = world.spawn_from_blueprint(
            "a".to_string(),
            bp,
            "task",
            vec![crate::pipeline::ResolvedStage {
                provider_name: "script".to_string(),
                model: "m".to_string(),
                tools: vec![],
            }],
            true,
        );
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn wake_handle_and_run_until_idle_bound_are_exposed() {
        // Exercises the wake handle accessor and the max-waits safety bound on a
        // world with an agent parked on an in-flight inference that never
        // resolves within the bound (script returns after we stop waiting).
        let mut world = build_world(registry_with(vec![with_tool("c1", "do"), text("done")]));
        let _ = world.wake_handle();
        let e = spawn(&mut world);
        world.run_until_idle(0).await; // bound 0 ⇒ no extra waits
        // With no waits allowed we may not have observed completion yet; drain.
        world.run_until_idle(20).await;
        assert_eq!(world.agent_status(e), Some(AgentStatus::Complete));
    }
}
