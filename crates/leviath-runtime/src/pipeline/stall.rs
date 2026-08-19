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

/// The clock the watchdog measures stall ages against.
///
/// Absent in production, where the wall clock is the only sensible answer. It
/// exists so a test can pin the instant the watchdog reads, and that matters
/// more than it looks: a stall's age is the gap between *two* clock reads - the
/// one that stamped `since` and the one this system does - so a second boundary
/// falling between them shifts every age by one. That is enough to flip a case
/// deliberately sitting one second inside the grace period, which turns a
/// boundary test into a coin toss that lands wrong on a loaded runner.
#[derive(Resource, Debug, Clone, Copy)]
pub struct StallClock(
    /// Returns Unix seconds. A bare `fn` rather than a boxed closure so the
    /// resource stays `Copy` and costs nothing when it is absent.
    pub fn() -> i64,
);

/// Wall-clock seconds since the Unix epoch: what the watchdog reads when no
/// [`StallClock`] pins it.
fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
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

/// What `fail_stalled_dispatch` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type StalledDispatchQuery = (
    Entity,
    &'static DispatchStall,
    &'static StageInference,
    &'static mut AgentState,
    Option<&'static mut StageIoBuffer>,
    Option<&'static crate::persistence::RunMetadata>,
);

/// A run parked until the machine is fixed, and what to do about it.
///
/// The message is the same one that used to be the run's epitaph. It is now
/// attached to a run that is still alive, which is the whole change: every
/// reason this marker exists for is deterministic, outside the run's control,
/// and undone by one edit somewhere else.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PausedForSetup {
    /// Which kind of problem, so a client can offer the right remedy rather
    /// than match on the sentence.
    pub blocker: leviath_core::run_meta::SetupBlocker,
    /// What a person has to do before `lev resume` will get anywhere.
    pub remedy: String,
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
pub fn fail_stalled_dispatch(
    mut agents: Query<StalledDispatchQuery>,
    timeout: Option<Res<StallTimeout>>,
    clock: Option<Res<StallClock>>,
    circuits: Option<Res<super::circuit::ProviderCircuits>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    let limit = timeout.map(|t| t.0).unwrap_or(DEFAULT_STALL_TIMEOUT_SECS);
    if limit == 0 {
        return; // watchdog disabled
    }
    let now = clock.map_or_else(now_secs, |c| (c.0)());
    for (entity, stall, si, mut state, buffer, md) in agents.iter_mut() {
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
        // Everything that reaches here is deterministic and outside the run's
        // control: a provider that is not configured, a key that was rejected,
        // an account with no credits. One edit elsewhere undoes any of them,
        // and the run's context is intact, so ending it would throw the work
        // away to punish somebody for a typo. It waits instead.
        // Which kind of problem, from the breaker's own record of why each
        // provider went out of service. The three have three different fixes,
        // so they are three different answers rather than one "unavailable".
        use leviath_core::run_meta::SetupBlocker;
        let last_reason = circuits
            .as_ref()
            .and_then(|c| c.last_reason(&si.provider_name));
        let blocker = match stall.reason {
            StallReason::ProviderCircuitOpen => match last_reason {
                Some(leviath_providers::UnavailableReason::CreditsExhausted) => {
                    SetupBlocker::CreditsExhausted
                }
                Some(leviath_providers::UnavailableReason::AuthFailed) => SetupBlocker::AuthFailed,
                Some(leviath_providers::UnavailableReason::Forbidden) => SetupBlocker::Forbidden,
                // Unreachable, or nothing recorded: the account and the key
                // are both fine as far as anyone knows, so neither screen is
                // the right one to send somebody to.
                _ => SetupBlocker::ProvidersUnavailable,
            },
            _ => SetupBlocker::ProviderMissing,
        };
        // What to fix, and - separately - whether this run will still be there
        // to pick it up. The two were one string, so a run the branch below
        // *failed* was told to `lev resume` it, which is not a thing that
        // works on a failed run (issue #456).
        let remedy = match blocker {
            SetupBlocker::CreditsExhausted => {
                format!(
                    "out of credits on '{}': top up the account",
                    si.provider_name
                )
            }
            SetupBlocker::AuthFailed => format!(
                "'{}' rejected the API key: replace it with `lev setup`",
                si.provider_name
            ),
            SetupBlocker::Forbidden => format!(
                "'{}' will not serve this model to that key: check the account's \
                 plan and model permissions",
                si.provider_name
            ),
            _ => stall.reason.give_up_message(&si.provider_name),
        };
        // Every run parks, unattended included. This used to fail an
        // unattended one on the reasoning that a scheduler watches for a
        // terminal status and would wait for ever for one that never comes.
        // That undersold harnesses: `paused` is visible in `meta.json` and
        // `lev ps --json`, and one that can top up an account and `lev resume`
        // gets its work back. One that cannot is no worse off than before -
        // it cancels the run, which is a decision it can make in a second,
        // where a failed run's work is gone for good (issue #456).
        tracing::warn!(
            provider = %si.provider_name,
            reason = stall.reason.label(),
            stalled_secs = now.saturating_sub(stall.since),
            unattended = md.is_some_and(|m| m.unattended),
            "pausing a run until the machine is fixed"
        );
        // Paused, so resume is the truthful next step and is worth naming.
        let message = format!("{remedy}, then `lev resume` this run");
        if let Some(mut buffer) = buffer {
            buffer.logs.push((0, format!("[paused] {message}")));
        }
        state.status = AgentStatus::Paused;
        // `ReadyToInfer` stays on purpose: the retry is already staged, so a
        // resume re-dispatches rather than rebuilding anything.
        commands
            .entity(entity)
            .insert(PausedForSetup {
                blocker,
                remedy: message,
            })
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
            output: None,
        }
    }

    /// The instant these tests pretend it is, on both sides of the comparison.
    ///
    /// Arbitrary, and deliberately not the wall clock: see [`StallClock`] for
    /// why reading it twice makes a boundary test flaky.
    const NOW: i64 = 1_700_000_000;

    /// A stall that started `age` seconds ago and is still being refreshed.
    fn stalled_for(reason: StallReason, age: i64) -> DispatchStall {
        DispatchStall {
            since: NOW - age,
            last_seen: NOW,
            reason,
        }
    }

    /// Spawn an agent that has been stalled for `age` seconds for `reason`.
    ///
    /// Attended, because that is the ordinary case: a person started it and
    /// can fix whatever is wrong. [`spawn_stalled_unattended`] is the other.
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

    /// The same, launched by something that is not watching.
    fn spawn_stalled_unattended(world: &mut World, reason: StallReason, age: i64) -> Entity {
        let e = spawn_stalled(world, reason, age);
        world.entity_mut(e).insert(run_metadata(true));
        e
    }

    /// Run metadata carrying only the field the watchdog reads.
    fn run_metadata(unattended: bool) -> crate::persistence::RunMetadata {
        crate::persistence::RunMetadata {
            run_id: "r".to_string(),
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
            unattended,
            read_paths: None,
            output_request: None,
            model_override: None,
        }
    }

    /// Assert a run is parked for setup, with `remedy` naming what to do.
    fn assert_paused_for_setup(world: &World, e: Entity, remedy: &str) {
        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Paused,
            "a fixable problem parks the run rather than ending it"
        );
        let marker = world
            .get::<PausedForSetup>(e)
            .expect("a parked run says what to do");
        assert!(marker.remedy.contains(remedy), "{}", marker.remedy);
        // The retry stays staged, so a resume re-dispatches rather than
        // rebuilding anything.
        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<DispatchStall>(e).is_none());
    }

    /// Run the watchdog with the clock pinned to [`NOW`], so an age of `n` is
    /// exactly `n` and the grace boundary can be asserted to the second.
    fn run(world: &mut World) {
        world.insert_resource(StallClock(|| NOW));
        run_on_the_wall_clock(world);
    }

    /// Run it the way production does, with no clock pinned.
    fn run_on_the_wall_clock(world: &mut World) {
        let mut schedule = Schedule::default();
        schedule.add_systems(fail_stalled_dispatch);
        schedule.run(world);
    }

    /// A provider that is not configured is one config edit away from being
    /// configured, so the run waits for that edit instead of dying for it.
    #[test]
    fn a_provider_that_will_never_resolve_parks_the_run_for_a_person() {
        let mut world = World::new();
        world.insert_resource(StallTimeout(60));
        let e = spawn_stalled(&mut world, StallReason::ProviderMissing, 61);

        run(&mut world);

        assert_paused_for_setup(&world, e, "not configured");
        // The operator sees why in the stage log the dashboard renders.
        let logs = &world.get::<StageIoBuffer>(e).unwrap().logs;
        assert!(
            logs.iter().any(|(_, line)| line.starts_with("[paused]")),
            "expected a [paused] log line, got: {logs:?}"
        );
    }

    /// Nobody watching is not a reason to throw the work away.
    ///
    /// This used to fail, on the reasoning that a scheduler polls for a
    /// terminal status and would wait for ever for one that never came. A
    /// benchmark round lost 31 runs and a tier to that (issue #456): the
    /// account needed topping up, which is one edit elsewhere, and every one
    /// of those runs had already done real work. A harness that can rescue a
    /// paused run now gets to; one that cannot cancels it, which costs a
    /// second, where a failed run's work is gone.
    #[test]
    fn an_unattended_run_parks_like_any_other_rather_than_losing_its_work() {
        let mut world = World::new();
        world.insert_resource(StallTimeout(60));
        let e = spawn_stalled_unattended(&mut world, StallReason::ProviderMissing, 61);

        run(&mut world);

        assert_paused_for_setup(&world, e, "not configured");
        // The staged retry survives, so a resume re-dispatches rather than
        // rebuilding - the whole point of parking instead of failing.
        assert!(world.get::<ReadyToInfer>(e).is_some());
        let logs = &world.get::<StageIoBuffer>(e).unwrap().logs;
        assert!(
            logs.iter().any(|(_, line)| line.starts_with("[paused]")),
            "expected a [paused] log line, got: {logs:?}"
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
    fn the_grace_period_ends_the_second_it_is_reached() {
        // `<` rather than `<=`, so an age equal to the limit is already out of
        // grace. Only worth asserting because the clock is pinned - against the
        // wall clock this is the exact case a one-second drift inverts.
        let mut world = World::new();
        world.insert_resource(StallTimeout(60));
        let e = spawn_stalled(&mut world, StallReason::ProviderMissing, 60);

        run(&mut world);

        assert_paused_for_setup(&world, e, "ghost");
    }

    #[test]
    fn nothing_pinning_the_clock_means_the_wall_clock() {
        // Production inserts no `StallClock`. The age here is far enough past
        // the limit that no drift between the two reads can change the verdict.
        let mut world = World::new();
        world.insert_resource(StallTimeout(60));
        let now = chrono::Utc::now().timestamp();
        let e = world
            .spawn((
                agent_state(),
                stage_inference(),
                DispatchStall {
                    since: now - 10_000,
                    last_seen: now,
                    reason: StallReason::ProviderMissing,
                },
                ReadyToInfer,
            ))
            .id();

        run_on_the_wall_clock(&mut world);

        assert_paused_for_setup(&world, e, "ghost");
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
        assert_paused_for_setup(&world, past, "ghost");
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

        assert_paused_for_setup(&world, e, "ghost");
    }

    #[test]
    fn a_stall_that_stopped_being_refreshed_is_discarded() {
        // The agent left the ready state for some unrelated reason (a stuck
        // edge, an iteration cap) and came back. It must not be judged on a
        // wait it was not actually doing.
        let mut world = World::new();
        let e = world
            .spawn((
                agent_state(),
                stage_inference(),
                DispatchStall {
                    since: NOW - 10_000,
                    last_seen: NOW - STALL_FRESHNESS_SECS - 1,
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

        assert_paused_for_setup(&world, e, "out of service");
    }

    #[test]
    fn a_run_out_of_credits_is_paused_for_a_resume_not_failed() {
        // Issue #413: exhausted credits are an account state the operator can
        // fix, so the watchdog pauses the run instead of ending it. The
        // `ReadyToInfer` marker stays, so a resume re-dispatches the same
        // inference.
        let mut world = World::new();
        world.insert_resource(StallTimeout(60));
        let mut circuits = super::super::circuit::ProviderCircuits::default();
        let policy = super::super::circuit::CircuitPolicy::default();
        for i in 0..3 {
            circuits.record_failure(
                "ghost",
                leviath_providers::UnavailableReason::CreditsExhausted,
                NOW - 3 + i,
                &policy,
            );
        }
        world.insert_resource(circuits);
        let e = spawn_stalled(&mut world, StallReason::ProviderCircuitOpen, 61);

        run(&mut world);

        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Paused
        );
        assert!(
            world.get::<ReadyToInfer>(e).is_some(),
            "the retry is staged"
        );
        assert!(world.get::<DispatchStall>(e).is_none());
        let logs = &world.get::<StageIoBuffer>(e).unwrap().logs;
        let line = logs
            .iter()
            .map(|(_, l)| l.as_str())
            .find(|l| l.starts_with("[paused]"))
            .expect("the pause is written to the stage log");
        assert!(line.contains("out of credits"), "{line}");
        assert!(line.contains("lev resume"), "{line}");
    }

    #[test]
    fn the_credits_pause_copes_without_a_stage_log_buffer() {
        // `StageIoBuffer` is optional on the query, so the pause has to land
        // even when there is no stage log to explain it in.
        let mut world = World::new();
        world.insert_resource(StallTimeout(60));
        let mut circuits = super::super::circuit::ProviderCircuits::default();
        let policy = super::super::circuit::CircuitPolicy::default();
        for i in 0..3 {
            circuits.record_failure(
                "ghost",
                leviath_providers::UnavailableReason::CreditsExhausted,
                NOW - 3 + i,
                &policy,
            );
        }
        world.insert_resource(circuits);
        let e = world
            .spawn((
                agent_state(),
                stage_inference(),
                stalled_for(StallReason::ProviderCircuitOpen, 61),
                ReadyToInfer,
            ))
            .id();

        run(&mut world);

        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Paused
        );
    }

    /// Each way a provider can go out of service gets its own answer, because
    /// each has its own fix: top up, replace the key, or check the plan. A
    /// client that had only "unavailable" would have to send everyone to the
    /// same screen and hope.
    #[test]
    fn each_kind_of_provider_failure_names_its_own_remedy() {
        use leviath_core::run_meta::SetupBlocker;
        let cases = [
            (
                leviath_providers::UnavailableReason::CreditsExhausted,
                SetupBlocker::CreditsExhausted,
                "top up",
            ),
            (
                leviath_providers::UnavailableReason::AuthFailed,
                SetupBlocker::AuthFailed,
                "rejected the API key",
            ),
            (
                leviath_providers::UnavailableReason::Forbidden,
                SetupBlocker::Forbidden,
                "will not serve this model",
            ),
            (
                // Nothing anyone can point at: neither the account nor the key
                // is known to be wrong, so neither screen is the right one.
                leviath_providers::UnavailableReason::Unreachable,
                SetupBlocker::ProvidersUnavailable,
                "out of service",
            ),
        ];
        for (reason, expected, remedy) in cases {
            let mut world = World::new();
            world.insert_resource(StallTimeout(60));
            let mut circuits = super::super::circuit::ProviderCircuits::default();
            let policy = super::super::circuit::CircuitPolicy::default();
            circuits.record_failure("ghost", reason, NOW - 1, &policy);
            world.insert_resource(circuits);
            let e = spawn_stalled(&mut world, StallReason::ProviderCircuitOpen, 61);

            run(&mut world);

            assert_paused_for_setup(&world, e, remedy);
            assert_eq!(
                world.get::<PausedForSetup>(e).unwrap().blocker,
                expected,
                "{reason:?}"
            );
        }
    }

    /// `StageIoBuffer` is optional on the query, so the unattended failure has
    /// to land even when there is no stage log to explain it in.
    #[test]
    fn an_unattended_park_copes_without_a_stage_log_buffer() {
        let mut world = World::new();
        world.insert_resource(StallTimeout(60));
        let e = world
            .spawn((
                agent_state(),
                stage_inference(),
                stalled_for(StallReason::ProviderMissing, 61),
                run_metadata(true),
                ReadyToInfer,
            ))
            .id();

        run(&mut world);

        let status = format!("{:?}", world.get::<AgentState>(e).unwrap().status);
        assert!(status.contains("Paused"), "{status}");
    }

    /// A provider that was never configured is a different fix again, and the
    /// one case that does not go through the breaker at all.
    #[test]
    fn a_missing_provider_is_its_own_kind_of_blocker() {
        use leviath_core::run_meta::SetupBlocker;
        let mut world = World::new();
        world.insert_resource(StallTimeout(60));
        let e = spawn_stalled(&mut world, StallReason::ProviderMissing, 61);

        run(&mut world);

        assert_eq!(
            world.get::<PausedForSetup>(e).unwrap().blocker,
            SetupBlocker::ProviderMissing
        );
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
