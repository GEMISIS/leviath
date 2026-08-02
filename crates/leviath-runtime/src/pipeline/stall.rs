//! The dispatch-stall watchdog: fail a run that is runnable but can never run.

use super::*;

/// Why a dispatch system declined to start work for an agent this tick.
///
/// The two cases look identical from the outside - the agent keeps its
/// `ReadyToInfer` marker either way - but they are opposites in kind, which is
/// what the watchdog acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallReason {
    /// The stage names a provider that is not in the registry. Nothing the
    /// runtime does will change that: no work is in flight to finish, no permit
    /// will free up. Only editing the config and restarting the daemon (or
    /// dropping in the matching `.rhai` script) can.
    ProviderMissing,
    /// The model's inference pool is full. This is ordinary backpressure and
    /// resolves itself: every permit is held by a job that the job timeout
    /// bounds, and releasing one wakes the driver.
    PoolFull,
    /// Every provider this stage could use has an open circuit: they have each
    /// failed enough consecutive times to be taken out of service, and the
    /// stage has no candidate left to move to (issue #201).
    ///
    /// Unlike `PoolFull` this will not clear on its own within a tick or two -
    /// somebody has to top up an account or fix a key - so the watchdog fails
    /// it like `ProviderMissing`. Unlike `ProviderMissing` it *can* recover
    /// without a restart, which is what the grace period is for.
    ProviderCircuitOpen,
}

impl StallReason {
    /// A short label for logs.
    pub(crate) fn label(self) -> &'static str {
        match self {
            StallReason::ProviderMissing => "provider-missing",
            StallReason::PoolFull => "pool-full",
            StallReason::ProviderCircuitOpen => "provider-circuit-open",
        }
    }

    /// Whether the runtime can resolve this on its own given time.
    ///
    /// `PoolFull` clears itself the moment a permit frees, so failing a run for
    /// it would be failing backpressure. The other two need a person, and a run
    /// that waits on one for ever reads as healthy while going nowhere.
    fn needs_a_person(self) -> bool {
        match self {
            StallReason::ProviderMissing | StallReason::ProviderCircuitOpen => true,
            StallReason::PoolFull => false,
        }
    }

    /// The operator-facing explanation used when the watchdog gives up.
    fn give_up_message(self, provider: &str) -> String {
        match self {
            StallReason::ProviderCircuitOpen => format!(
                "every provider this stage can use is out of service (last was \
                 '{provider}'), so this run has nowhere to go; check the account's \
                 credits and API key, or add another provider to \
                 `[providers] fallback_order`"
            ),
            // `PoolFull` never reaches the watchdog (see `needs_a_person`), so
            // the missing-provider wording covers the remaining case.
            _ => format!(
                "provider '{provider}' is not configured, so this run has no way to \
                 go on; add it to config.toml (or run `lev setup`) and restart the daemon"
            ),
        }
    }
}

/// An agent that was ready to work but whose dispatch declined, and since when.
///
/// Attached by the dispatch systems when they decline, refreshed while the same
/// reason persists, and removed the moment work is dispatched - so its presence
/// means "runnable right now, and has been going nowhere since `since`".
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchStall {
    /// Unix seconds when this stall started (not when it was last observed, so
    /// the age is the whole stall).
    pub since: i64,
    /// Unix seconds of the most recent decline.
    ///
    /// This is what keeps the record honest. An agent can leave the ready state
    /// for reasons that have nothing to do with dispatch - a stuck edge, an
    /// iteration cap - and come back later; without a freshness stamp it would
    /// return carrying an ancient `since` and be judged on a wait it was not
    /// actually doing. A record that stops being refreshed simply expires.
    pub last_seen: i64,
    /// What is holding the agent up.
    pub reason: StallReason,
}

/// How long a [`DispatchStall`] stays meaningful without being refreshed.
///
/// The dispatch systems re-stamp it on every tick they decline, and the host
/// re-drives at least once per `DEFAULT_REDRIVE_INTERVAL` (30s), so a live
/// stall is never more than one interval stale. This is comfortably above that
/// so an ongoing stall is never mistaken for an abandoned record; anything
/// older than this really does describe a wait that has since ended.
pub(crate) const STALL_FRESHNESS_SECS: i64 = 120;

/// How long a `ProviderMissing` stall may last before the run is failed.
///
/// A world resource rather than a constant because the daemon serves it from
/// `[limits] stall_timeout_secs`. Zero disables the watchdog.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct StallTimeout(pub u64);

impl Default for StallTimeout {
    fn default() -> Self {
        Self(DEFAULT_STALL_TIMEOUT_SECS)
    }
}

/// Default grace period before an unresolvable stall fails its run.
///
/// Long enough that a provider arriving late - a `.rhai` script dropped into the
/// providers directory resolves on the next dispatch - still rescues the run,
/// short enough that an operator watching `lev ps` gets an answer rather than a
/// run that claims to be working.
pub const DEFAULT_STALL_TIMEOUT_SECS: u64 = 60;

/// Record that an agent's dispatch declined for `reason`, preserving the start
/// time of an ongoing stall of the same kind.
///
/// The clock restarts unless this continues a stall that is both the *same
/// kind* and still fresh. A changed reason is a different problem and deserves
/// its own grace period rather than inheriting the age of the old one; a stale
/// record describes a wait that already ended (see
/// [`STALL_FRESHNESS_SECS`]).
pub(crate) fn note_stall(
    existing: Option<&DispatchStall>,
    reason: StallReason,
    now: i64,
) -> DispatchStall {
    let since = match existing {
        Some(prev)
            if prev.reason == reason
                && now.saturating_sub(prev.last_seen) <= STALL_FRESHNESS_SECS =>
        {
            prev.since
        }
        _ => now,
    };
    DispatchStall {
        since,
        last_seen: now,
        reason,
    }
}

/// Dispatch-stall watchdog: fail any agent whose dispatch has been declining for
/// an unresolvable reason longer than [`StallTimeout`].
///
/// This is the backstop under issue #190. A stage pointing at a provider that
/// isn't registered leaves the agent `Active` and `ReadyToInfer` with nothing in
/// flight - so from the outside it reads as a healthy running run, for ever, at
/// iteration 0. The daemon now re-ticks on a heartbeat, which makes the retry
/// real, but retrying a provider that will never exist just means failing
/// quietly for ever instead of loudly once.
///
/// Only [`StallReason::ProviderMissing`] is failed. A full pool is deliberately
/// exempt: it is what backpressure is supposed to look like, and a run waiting
/// its turn behind seven long inferences is working exactly as intended.
///
/// What makes this safe from false positives is *which* agents can carry a
/// [`DispatchStall`] at all. Only a dispatch system that declined attaches one,
/// and dispatching removes it - so an agent holding one has nothing
/// outstanding. A fifteen-minute inference is `AwaitingInference` with no stall
/// record, and is never a candidate here.
#[allow(clippy::type_complexity)]
pub fn fail_stalled_dispatch(
    mut agents: Query<(
        Entity,
        &DispatchStall,
        &StageInference,
        &mut AgentState,
        Option<&mut StageIoBuffer>,
    )>,
    timeout: Option<Res<StallTimeout>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    let limit = timeout.map(|t| t.0).unwrap_or(DEFAULT_STALL_TIMEOUT_SECS);
    if limit == 0 {
        return; // watchdog disabled
    }
    let now = chrono::Utc::now().timestamp();
    for (entity, stall, si, mut state, buffer) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if state.status != AgentStatus::Active || !stall.reason.needs_a_person() {
            continue;
        }
        if now.saturating_sub(stall.last_seen) > STALL_FRESHNESS_SECS {
            // The wait this describes has ended; nothing to act on.
            tracing::debug!(
                reason = stall.reason.label(),
                "discarding a dispatch stall that stopped being refreshed"
            );
            commands.entity(entity).remove::<DispatchStall>();
            continue;
        }
        if now.saturating_sub(stall.since) < limit as i64 {
            continue; // still inside the grace period
        }
        let message = stall.reason.give_up_message(&si.provider_name);
        tracing::error!(
            provider = %si.provider_name,
            reason = stall.reason.label(),
            stalled_secs = now.saturating_sub(stall.since),
            "failing a run whose provider will never resolve"
        );
        if let Some(mut buffer) = buffer {
            buffer.logs.push((0, format!("[stalled] {message}")));
        }
        state.status = AgentStatus::Error { message };
        commands
            .entity(entity)
            .remove::<ReadyToInfer>()
            .remove::<DispatchStall>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn stage_inference() -> StageInference {
        StageInference {
            provider_name: "ghost".to_string(),
            model: "m".to_string(),
            tools: vec![],
            tool_filter: None,
            fallbacks: Vec::new(),
        }
    }

    /// A stall that started `age` seconds ago and is still being refreshed.
    fn stalled_for(reason: StallReason, age: i64) -> DispatchStall {
        let now = chrono::Utc::now().timestamp();
        DispatchStall {
            since: now - age,
            last_seen: now,
            reason,
        }
    }

    /// Spawn an agent that has been stalled for `age` seconds for `reason`.
    fn spawn_stalled(world: &mut World, reason: StallReason, age: i64) -> Entity {
        world
            .spawn((
                agent_state(),
                stage_inference(),
                stalled_for(reason, age),
                StageIoBuffer::default(),
                ReadyToInfer,
            ))
            .id()
    }

    fn run(world: &mut World) {
        let mut schedule = Schedule::default();
        schedule.add_systems(fail_stalled_dispatch);
        schedule.run(world);
    }

    #[test]
    fn a_provider_that_will_never_resolve_fails_the_run() {
        let mut world = World::new();
        world.insert_resource(StallTimeout(60));
        let e = spawn_stalled(&mut world, StallReason::ProviderMissing, 61);

        run(&mut world);

        let status = &world.get::<AgentState>(e).unwrap().status;
        assert!(
            matches!(status, AgentStatus::Error { message }
                if message.contains("ghost") && message.contains("not configured")),
            "got: {status:?}"
        );
        // Taken out of dispatch, and the stall record is spent.
        assert!(world.get::<ReadyToInfer>(e).is_none());
        assert!(world.get::<DispatchStall>(e).is_none());
        // The operator sees why in the stage log the dashboard renders.
        let logs = &world.get::<StageIoBuffer>(e).unwrap().logs;
        assert!(
            logs.iter().any(|(_, line)| line.starts_with("[stalled]")),
            "expected a [stalled] log line, got: {logs:?}"
        );
    }

    #[test]
    fn a_stall_inside_the_grace_period_is_left_alone() {
        let mut world = World::new();
        world.insert_resource(StallTimeout(60));
        let e = spawn_stalled(&mut world, StallReason::ProviderMissing, 59);

        run(&mut world);

        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Active
        );
        assert!(world.get::<ReadyToInfer>(e).is_some());
    }

    #[test]
    fn a_full_pool_is_backpressure_and_is_never_failed() {
        let mut world = World::new();
        world.insert_resource(StallTimeout(60));
        // Far past the grace period: waiting behind long inferences is fine.
        let e = spawn_stalled(&mut world, StallReason::PoolFull, 10_000);

        run(&mut world);

        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Active
        );
        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<DispatchStall>(e).is_some());
    }

    #[test]
    fn a_zero_timeout_disables_the_watchdog() {
        let mut world = World::new();
        world.insert_resource(StallTimeout(0));
        let e = spawn_stalled(&mut world, StallReason::ProviderMissing, 10_000);

        run(&mut world);

        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Active
        );
    }

    #[test]
    fn a_world_without_the_resource_uses_the_default_timeout() {
        // Test worlds and `lev run` don't insert `StallTimeout`.
        let mut world = World::new();
        let inside = spawn_stalled(
            &mut world,
            StallReason::ProviderMissing,
            DEFAULT_STALL_TIMEOUT_SECS as i64 - 1,
        );
        let past = spawn_stalled(
            &mut world,
            StallReason::ProviderMissing,
            DEFAULT_STALL_TIMEOUT_SECS as i64 + 1,
        );

        run(&mut world);

        assert_eq!(
            world.get::<AgentState>(inside).unwrap().status,
            AgentStatus::Active
        );
        let status = &world.get::<AgentState>(past).unwrap().status;
        assert!(
            matches!(status, AgentStatus::Error { message } if message.contains("ghost")),
            "got: {status:?}"
        );
    }

    #[test]
    fn a_non_active_agent_is_left_to_its_own_status() {
        // A paused run is not stalled - it is stopped on purpose, and resuming
        // it must not find it failed.
        let mut world = World::new();
        let e = spawn_stalled(&mut world, StallReason::ProviderMissing, 10_000);
        world.get_mut::<AgentState>(e).unwrap().status = AgentStatus::Paused;

        run(&mut world);

        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Paused
        );
    }

    #[test]
    fn an_agent_without_a_stage_log_still_fails() {
        // `StageIoBuffer` is optional (test worlds, `lev run`).
        let mut world = World::new();
        let e = world
            .spawn((
                agent_state(),
                stage_inference(),
                stalled_for(StallReason::ProviderMissing, 10_000),
                ReadyToInfer,
            ))
            .id();

        run(&mut world);

        let status = &world.get::<AgentState>(e).unwrap().status;
        assert!(
            matches!(status, AgentStatus::Error { message } if message.contains("ghost")),
            "got: {status:?}"
        );
    }

    #[test]
    fn a_stall_that_stopped_being_refreshed_is_discarded() {
        // The agent left the ready state for some unrelated reason (a stuck
        // edge, an iteration cap) and came back. It must not be judged on a
        // wait it was not actually doing.
        let mut world = World::new();
        let now = chrono::Utc::now().timestamp();
        let e = world
            .spawn((
                agent_state(),
                stage_inference(),
                DispatchStall {
                    since: now - 10_000,
                    last_seen: now - STALL_FRESHNESS_SECS - 1,
                    reason: StallReason::ProviderMissing,
                },
                ReadyToInfer,
            ))
            .id();

        run(&mut world);

        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Active
        );
        assert!(
            world.get::<DispatchStall>(e).is_none(),
            "the spent record is cleared rather than left to mislead"
        );
    }

    #[test]
    fn note_stall_continues_a_live_stall_and_restarts_otherwise() {
        // A continuing stall keeps its start time, so the age is the whole wait.
        let first = note_stall(None, StallReason::PoolFull, 100);
        assert_eq!((first.since, first.last_seen), (100, 100));
        let still = note_stall(Some(&first), StallReason::PoolFull, 120);
        assert_eq!(still.since, 100, "an ongoing stall keeps its clock");
        assert_eq!(still.last_seen, 120, "but records that it is still live");
        // A different reason is a different problem: it gets its own grace.
        let changed = note_stall(Some(&first), StallReason::ProviderMissing, 120);
        assert_eq!(changed.since, 120);
        assert_eq!(changed.reason, StallReason::ProviderMissing);
        // So does a stall that went unobserved long enough to have ended.
        let resumed = note_stall(
            Some(&first),
            StallReason::PoolFull,
            100 + STALL_FRESHNESS_SECS + 1,
        );
        assert_eq!(resumed.since, 100 + STALL_FRESHNESS_SECS + 1);
    }

    #[test]
    fn stall_reasons_have_labels() {
        assert_eq!(StallReason::ProviderMissing.label(), "provider-missing");
        assert_eq!(StallReason::PoolFull.label(), "pool-full");
        assert_eq!(
            StallReason::ProviderCircuitOpen.label(),
            "provider-circuit-open"
        );
    }

    #[test]
    fn only_the_reasons_a_person_must_fix_are_failed() {
        // Failing `PoolFull` would be failing backpressure.
        assert!(StallReason::ProviderMissing.needs_a_person());
        assert!(StallReason::ProviderCircuitOpen.needs_a_person());
        assert!(!StallReason::PoolFull.needs_a_person());
    }

    #[test]
    fn a_run_with_every_provider_out_of_service_is_failed_not_left_running() {
        // The end state of issue #201: nothing left to fail over to. Waiting
        // for ever reads as a healthy run that is going nowhere.
        let mut world = World::new();
        world.insert_resource(StallTimeout(60));
        let e = spawn_stalled(&mut world, StallReason::ProviderCircuitOpen, 61);

        run(&mut world);

        let status = &world.get::<AgentState>(e).unwrap().status;
        assert!(
            matches!(status, AgentStatus::Error { message }
                if message.contains("out of service") && message.contains("fallback_order")),
            "got: {status:?}"
        );
        assert!(world.get::<ReadyToInfer>(e).is_none());
    }

    #[test]
    fn an_open_circuit_inside_the_grace_period_gets_its_chance_to_recover() {
        // Unlike a missing provider, this one can come back on its own once
        // the cooldown lets a probe through, so the grace period matters.
        let mut world = World::new();
        world.insert_resource(StallTimeout(60));
        let e = spawn_stalled(&mut world, StallReason::ProviderCircuitOpen, 59);

        run(&mut world);

        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Active
        );
    }

    #[test]
    fn the_give_up_message_names_the_provider() {
        let missing = StallReason::ProviderMissing.give_up_message("ghost");
        assert!(missing.contains("ghost") && missing.contains("not configured"));
        let open = StallReason::ProviderCircuitOpen.give_up_message("openrouter");
        assert!(open.contains("openrouter") && open.contains("out of service"));
        // `PoolFull` never reaches the watchdog, but the arm must still answer.
        assert!(StallReason::PoolFull.give_up_message("x").contains("x"));
    }

    #[test]
    fn the_default_timeout_is_the_documented_grace_period() {
        assert_eq!(StallTimeout::default().0, DEFAULT_STALL_TIMEOUT_SECS);
    }
}
