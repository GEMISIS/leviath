//! Driving the world: one tick, ticks until nothing moves, and the panic
//! isolation that keeps one agent's bug from stopping every other agent.

use super::*;

impl PipelineWorld {
    /// Run one schedule tick over every agent, catching a panic from any system
    /// so one bad agent can't crash the daemon and take every other hosted agent
    /// with it.
    ///
    /// When the panic can be traced to a specific agent (the usual case - see
    /// `tick_scope`), that agent is failed with the panic message so it
    /// stops being driven, its run is persisted as errored, and the host reaps
    /// it. Without that, the world would re-tick the same unchanged state on
    /// every wake and panic again indefinitely.
    pub(crate) fn tick(&mut self) -> TickOutcome {
        let Err(panicked) = run_isolated(&mut self.schedule, &mut self.world) else {
            // A clean unwind doesn't mean a clean tick: work that ran on the
            // compute pool catches its own panics, since they can't unwind back
            // here, and leaves a marker instead.
            return self.fail_agents_panicked_in_parallel();
        };
        let message = panic_status_message(&panicked.message);
        match panicked.entity {
            Some(entity) if self.set_status(self.own(entity), AgentStatus::Error { message }) => {
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
            let _ = self.set_status(self.own(entity), status);
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

    /// Drive the schedule until a tick changes nothing (quiescence). Public so a
    /// host loop can interleave control operations between quiescent points.
    pub(crate) fn run_to_fixed_point(&mut self) {
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
    /// `shutdown` is signalled.
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
pub(super) fn panic_status_message(panic: &str) -> String {
    format!("internal error: a pipeline system panicked: {panic}")
}

/// A panic caught while ticking the schedule, and the agent it belongs to.
pub(super) struct TickPanic {
    /// The agent being processed when the panic fired, if the pipeline had
    /// recorded one (see [`crate::tick_scope`]).
    pub(super) entity: Option<Entity>,
    /// The panic payload rendered as text.
    pub(super) message: String,
}

/// Run a schedule over a world, catching a panic from any system so it can't
/// unwind the daemon's drive loop and take down every hosted agent.
///
/// The world may be partially updated after a panic: the panicking system's
/// buffered `Commands` are lost, but resources and components already written
/// are intact, so the caller can still fail the offending agent.
pub(super) fn run_isolated(schedule: &mut Schedule, world: &mut World) -> Result<(), TickPanic> {
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
/// seeing an agent we just failed. Replacing the executor outright is the
/// public API for forcing that rebuild: `set_executor` takes an executor
/// *instance* and unconditionally replaces `schedule.executor` with it (clearing
/// `executor_initialized` too), so the fresh `SingleThreadedExecutor` arrives
/// with an empty `completed_systems`.
fn reset_executor(schedule: &mut Schedule) {
    schedule.set_executor(bevy_ecs::schedule::SingleThreadedExecutor::new());
}
