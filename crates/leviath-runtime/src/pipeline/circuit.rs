//! Per-provider circuit breakers: stop hammering a provider that has told us,
//! repeatedly, that it cannot serve anyone.
//!
//! Failing over (see [`super::response::collect_inference`]) rescues one run.
//! It does nothing for the *next* run, which starts on the same dead provider
//! and burns its own failure discovering the same thing. At scale that is ten
//! consecutive workers, every one of them dying at iteration 0 against an
//! OpenRouter account with no credits left.
//!
//! So failures are counted per provider. Past a threshold the circuit opens and
//! dispatch stops choosing that provider at all, which turns a silent stream of
//! dead runs into one visible state an operator can act on (`lev ps`, the
//! `leviath.provider.circuit.open` gauge, and a `tracing::error!`).
//!
//! There is no half-open *state*, on purpose. `is_open` simply stops answering
//! true once the cooldown has elapsed, so the next dispatch is the probe: it
//! either succeeds and closes the circuit, or fails and re-opens it with a
//! fresh timestamp. One less state machine to keep correct.

use super::*;

use std::collections::HashMap;

use leviath_providers::{FailureKind, UnavailableReason};
use serde::{Deserialize, Serialize};

/// When to open a provider's circuit, and how long to leave it open.
///
/// A world resource rather than constants because the daemon serves it from
/// `[limits]`.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct CircuitPolicy {
    /// Consecutive provider-fatal failures before the circuit opens. Zero
    /// disables the breaker entirely, leaving only per-run failover.
    pub failures_before_open: u32,
    /// How long an open circuit is left alone before the next request is
    /// allowed through as a probe.
    pub cooldown_secs: u64,
}

/// Default consecutive failures before a provider's circuit opens.
///
/// Three rather than one: a single 402 can be a request that asked for more
/// output tokens than the remaining balance covers, which a smaller request
/// would survive. Three in a row is an account, not a request.
pub const DEFAULT_FAILURES_BEFORE_OPEN: u32 = 3;

/// How much more patience a provider gets when it is demonstrably there.
///
/// A provider that refuses the connection is out of service after
/// `failures_before_open`. One that accepts the connection and then answers
/// slowly, or stops mid-answer, gets this many times that budget before its
/// circuit opens.
///
/// Four rather than one because the two failures do not mean the same thing.
/// Nothing listening on the port is a fact about the provider and the next
/// request will fail the same way. A timeout is a fact about *one request*: the
/// usual cause is an oversized prompt against a busy server, not a dead one, and
/// three of those in a row is an ordinary afternoon on a large run. Opening the
/// circuit there takes a working provider away from every run on the box for the
/// whole cooldown, which is a self-inflicted outage.
///
/// Four rather than never because a genuinely wedged provider - accepting
/// connections and answering nothing - is still one that no run should be sent
/// to, and without a ceiling every run would keep discovering that the slow way.
///
/// Derived from the same knob rather than a second one, so `[limits]` keeps a
/// single dial and `failures_before_open = 0` still disables the breaker whole.
pub const SLOW_FAILURE_MULTIPLIER: u32 = 4;

/// Default time an open circuit waits before probing again.
///
/// Long enough that a drained account is not probed every few seconds, short
/// enough that topping it up brings the factory back without a daemon restart.
pub const DEFAULT_CIRCUIT_COOLDOWN_SECS: u64 = 300;

impl Default for CircuitPolicy {
    fn default() -> Self {
        Self {
            failures_before_open: DEFAULT_FAILURES_BEFORE_OPEN,
            cooldown_secs: DEFAULT_CIRCUIT_COOLDOWN_SECS,
        }
    }
}

/// One provider's failure record. Absent from [`ProviderCircuits`] means
/// healthy, so a success can simply drop the entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Circuit {
    /// Provider-fatal failures since the last success.
    pub consecutive_failures: u32,
    /// When the circuit opened, if it is open. `None` while the count is still
    /// below the threshold.
    pub opened_at: Option<i64>,
    /// What the provider last complained about, for the operator-facing text.
    pub reason: UnavailableReason,
}

/// What an open circuit looks like to a client (`lev ps`, `--json`, telemetry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCircuitState {
    /// The provider whose circuit is open.
    pub provider: String,
    /// Why it was taken out of service.
    pub reason: UnavailableReason,
    /// How many consecutive failures it has accumulated.
    pub consecutive_failures: u32,
    /// Seconds until the next probe is allowed through.
    pub retry_in_secs: u64,
}

/// Every provider's breaker state, as a world resource.
///
/// Written by the (serial) collect system and read by dispatch, so plain
/// `Res`/`ResMut` access is enough - no interior mutability, no locks.
#[derive(Resource, Debug, Clone, Default)]
pub struct ProviderCircuits(HashMap<String, Circuit>);

impl CircuitPolicy {
    /// Consecutive failures before a failure of `kind` opens the circuit.
    ///
    /// `None` - a provider-fatal failure that carries no kind, such as a 402 the
    /// provider stated outright - takes the strict threshold. Those are the
    /// provider telling us about itself, which is exactly what the breaker is
    /// for.
    pub(crate) fn threshold_for(&self, kind: Option<FailureKind>) -> u32 {
        match kind {
            Some(k) if k.provider_was_reached() => self
                .failures_before_open
                .saturating_mul(SLOW_FAILURE_MULTIPLIER),
            _ => self.failures_before_open,
        }
    }
}

impl ProviderCircuits {
    /// Count a provider-fatal failure against `provider`.
    ///
    /// `kind` decides how much patience the provider gets: one that accepted the
    /// connection and then answered slowly is given
    /// [`SLOW_FAILURE_MULTIPLIER`] times the budget of one that could not be
    /// reached at all. See [`CircuitPolicy::threshold_for`].
    ///
    /// Returns `true` on the transition into the open state, so the caller can
    /// log and alert exactly once rather than on every subsequent failure.
    pub(crate) fn record_failure(
        &mut self,
        provider: &str,
        reason: UnavailableReason,
        kind: Option<FailureKind>,
        now: i64,
        policy: &CircuitPolicy,
    ) -> bool {
        let entry = self.0.entry(provider.to_string()).or_insert(Circuit {
            consecutive_failures: 0,
            opened_at: None,
            reason,
        });
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        entry.reason = reason;
        let threshold = policy.threshold_for(kind);
        if threshold == 0 {
            return false; // breaker disabled; keep counting for the record
        }
        let was_open = entry.opened_at.is_some();
        if entry.consecutive_failures >= threshold {
            // Re-stamp on every failure at or past the threshold: a probe that
            // fails must restart the cooldown, not inherit the old one.
            entry.opened_at = Some(now);
        }
        !was_open && entry.opened_at.is_some()
    }

    /// Forget `provider`'s failures. Any success proves it is serving again.
    pub(crate) fn record_success(&mut self, provider: &str) {
        self.0.remove(provider);
    }

    /// The reason of the last recorded failure for `provider`, open or not.
    ///
    /// The stall watchdog asks this to tell "out of credits" apart from the
    /// other ways a provider leaves service: the former pauses the run for a
    /// resume instead of failing it.
    pub(crate) fn last_reason(&self, provider: &str) -> Option<UnavailableReason> {
        self.0.get(provider).map(|c| c.reason)
    }

    /// Forget every recorded failure, so the next dispatch is a real probe.
    ///
    /// Called on an explicit resume: the operator is saying conditions have
    /// changed (most often a top-up after credits ran out), and holding the
    /// retry until a cooldown lapses would make the resume look ignored.
    pub(crate) fn reset(&mut self) {
        self.0.clear();
    }

    /// Drop everything recorded against one provider. For a provider whose
    /// credentials just changed: the failures that opened its circuit were
    /// the old key's, and holding the new key out of service for the rest
    /// of the cooldown would make a fixed key look still broken.
    pub fn forget(&mut self, provider: &str) {
        self.0.remove(provider);
    }

    /// Whether `provider` should be skipped right now.
    ///
    /// False once the cooldown has elapsed, which is what makes the next
    /// request a probe without needing a distinct half-open state.
    pub(crate) fn is_open(&self, provider: &str, now: i64, policy: &CircuitPolicy) -> bool {
        self.0
            .get(provider)
            .and_then(|c| c.opened_at)
            .is_some_and(|at| now.saturating_sub(at) < policy.cooldown_secs as i64)
    }

    /// Every currently-open circuit, provider-sorted so the rendering is
    /// stable across ticks (a `HashMap` iteration order is not).
    pub(crate) fn open_circuits(
        &self,
        now: i64,
        policy: &CircuitPolicy,
    ) -> Vec<ProviderCircuitState> {
        let mut open: Vec<ProviderCircuitState> = self
            .0
            .iter()
            .filter_map(|(provider, c)| {
                let at = c.opened_at?;
                let elapsed = now.saturating_sub(at);
                let remaining = (policy.cooldown_secs as i64).saturating_sub(elapsed);
                (remaining > 0).then(|| ProviderCircuitState {
                    provider: provider.clone(),
                    reason: c.reason,
                    consecutive_failures: c.consecutive_failures,
                    retry_in_secs: remaining as u64,
                })
            })
            .collect();
        open.sort_by(|a, b| a.provider.cmp(&b.provider));
        open
    }
}

/// Move any ready agent off a provider whose circuit is open, before dispatch
/// gets to it.
///
/// This runs *serially*, unlike [`super::inference::dispatch_inference`], which
/// fans out over `par_iter` and so cannot take the `&mut StageInference` a swap
/// needs. Keeping the rotation here also means dispatch stays a pure decision:
/// by the time it looks at an agent, the agent is already pointed at the best
/// provider still standing.
///
/// An agent with nowhere left to go is left alone, and dispatch parks it on
/// [`super::StallReason::ProviderCircuitOpen`].
pub(crate) fn rotate_open_circuits(
    mut agents: Query<(Entity, &AgentState, &mut StageInference), With<super::ReadyToInfer>>,
    circuits: Option<Res<ProviderCircuits>>,
    policy: Option<Res<CircuitPolicy>>,
) {
    crate::tick_scope::clear();
    let Some(circuits) = circuits else {
        return; // no breaker installed
    };
    let policy = policy.map(|p| *p).unwrap_or_default();
    let now = chrono::Utc::now().timestamp();
    for (entity, state, mut si) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if state.status != crate::components::AgentStatus::Active {
            continue;
        }
        if !circuits.is_open(&si.provider_name, now, &policy) {
            continue;
        }
        // First candidate whose own circuit is closed. Everything skipped on
        // the way is dropped: it is no better than what we are leaving.
        let Some(next) = si
            .fallbacks
            .iter()
            .position(|e| !circuits.is_open(&e.provider, now, &policy))
        else {
            continue; // nowhere to go; dispatch will park it
        };
        let entry = si.fallbacks.remove(next);
        si.fallbacks.drain(..next);
        tracing::warn!(
            from_provider = %si.provider_name,
            to_provider = %entry.provider,
            to_model = %entry.model,
            "provider circuit is open; moving this run to the next candidate"
        );
        si.provider_name = entry.provider;
        si.model = entry.model;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> CircuitPolicy {
        CircuitPolicy {
            failures_before_open: 3,
            cooldown_secs: 300,
        }
    }

    fn fail(circuits: &mut ProviderCircuits, now: i64) -> bool {
        circuits.record_failure(
            "openrouter",
            UnavailableReason::CreditsExhausted,
            None,
            now,
            &policy(),
        )
    }

    /// Failures have to be *consecutive*, and a success in between is what
    /// makes them not.
    ///
    /// The existing cover only checked that a success closes an already-open
    /// circuit. The case that actually happens is quieter: a provider that
    /// times out now and then, works in between, and never has three bad calls
    /// in a row. Without the reset those add up over an afternoon and the
    /// provider is pulled for a fault it does not have.
    #[test]
    fn a_success_in_between_means_the_failures_are_not_consecutive() {
        let mut circuits = ProviderCircuits::default();
        // The strict threshold, so the count is the only thing under test.
        let dead = Some(FailureKind::ConnectionRefused);
        let hit = |c: &mut ProviderCircuits, now: i64| {
            c.record_failure("p", UnavailableReason::Unreachable, dead, now, &policy())
        };

        assert!(!hit(&mut circuits, 0));
        assert!(!hit(&mut circuits, 1));
        circuits.record_success("p");
        // Three more failures have now happened in total. Counting them all
        // would open the circuit here; counting them in a row does not.
        assert!(!hit(&mut circuits, 2), "the count restarted at the success");
        assert!(!circuits.is_open("p", 2, &policy()));

        assert!(!hit(&mut circuits, 3));
        circuits.record_success("p");
        assert!(!hit(&mut circuits, 4));
        assert!(
            !circuits.is_open("p", 4, &policy()),
            "five failures, never three in a row, and the provider stays in service"
        );

        // And an actual run of three still opens it, so the reset has not
        // disarmed the breaker.
        assert!(!hit(&mut circuits, 5));
        assert!(hit(&mut circuits, 6), "three in a row is still an outage");
    }

    /// A slow provider is one that answered the connection, so it keeps its
    /// place in the rotation far longer than one that refused it.
    ///
    /// This is the whole point of the change: three slow calls in a row is an
    /// ordinary afternoon on a large run, and opening the circuit there takes a
    /// working provider away from *every* run on the box for the cooldown.
    #[test]
    fn a_timeout_does_not_open_the_circuit_where_a_refusal_would() {
        let slow = Some(FailureKind::Timeout);
        let mut circuits = ProviderCircuits::default();
        for now in 0..3 {
            assert!(
                !circuits.record_failure("p", UnavailableReason::Unreachable, slow, now, &policy()),
                "a timeout must not open the circuit at the strict threshold"
            );
        }
        assert!(
            !circuits.is_open("p", 3, &policy()),
            "three slow calls is not an outage"
        );

        // The same three against a provider that could not be reached at all.
        let dead = Some(FailureKind::ConnectionRefused);
        let mut refused = ProviderCircuits::default();
        assert!(!refused.record_failure("p", UnavailableReason::Unreachable, dead, 0, &policy()));
        assert!(!refused.record_failure("p", UnavailableReason::Unreachable, dead, 1, &policy()));
        assert!(
            refused.record_failure("p", UnavailableReason::Unreachable, dead, 2, &policy()),
            "nothing listening is a fact about the provider, and opens at three"
        );
    }

    /// Patient, not infinite. A provider that accepts connections and answers
    /// nothing is still one no run should be sent to, so the slow threshold has
    /// a ceiling rather than being waived.
    #[test]
    fn a_wedged_provider_still_opens_its_circuit_eventually() {
        let slow = Some(FailureKind::Timeout);
        let mut circuits = ProviderCircuits::default();
        let threshold = policy().threshold_for(slow);
        assert_eq!(threshold, 12, "three strict, four times the rope");

        let mut opened_at = None;
        for now in 0..i64::from(threshold) {
            if circuits.record_failure("p", UnavailableReason::Unreachable, slow, now, &policy()) {
                opened_at = Some(now + 1);
            }
        }
        assert_eq!(
            opened_at,
            Some(i64::from(threshold)),
            "it opens on the patient threshold, not before and not never"
        );
        assert!(circuits.is_open("p", i64::from(threshold), &policy()));
    }

    /// A failure the provider stated outright carries no transport kind, and
    /// takes the strict threshold. Those are the provider telling us about
    /// itself, which is exactly what the breaker is for.
    #[test]
    fn a_stated_failure_keeps_the_strict_threshold() {
        assert_eq!(policy().threshold_for(None), 3);
        assert_eq!(
            policy().threshold_for(Some(FailureKind::ConnectionRefused)),
            3
        );
        assert_eq!(
            policy().threshold_for(Some(FailureKind::ConnectionDropped)),
            12
        );
    }

    /// Zero disables the breaker whole, and multiplying zero must not quietly
    /// re-enable it for the slow path.
    #[test]
    fn a_disabled_breaker_stays_disabled_for_both_thresholds() {
        let off = CircuitPolicy {
            failures_before_open: 0,
            cooldown_secs: 300,
        };
        assert_eq!(off.threshold_for(None), 0);
        assert_eq!(off.threshold_for(Some(FailureKind::Timeout)), 0);

        let mut circuits = ProviderCircuits::default();
        for now in 0..20 {
            assert!(!circuits.record_failure(
                "p",
                UnavailableReason::Unreachable,
                Some(FailureKind::Timeout),
                now,
                &off
            ));
        }
        assert!(!circuits.is_open("p", 20, &off));
    }

    #[test]
    fn the_circuit_opens_only_at_the_threshold() {
        let mut circuits = ProviderCircuits::default();
        assert!(!fail(&mut circuits, 0));
        assert!(!circuits.is_open("openrouter", 0, &policy()));
        assert!(!fail(&mut circuits, 1));
        assert!(!circuits.is_open("openrouter", 1, &policy()));
        // Third strike: opens, and says so exactly once.
        assert!(fail(&mut circuits, 2), "the transition is reported");
        assert!(circuits.is_open("openrouter", 2, &policy()));
        assert!(
            !fail(&mut circuits, 3),
            "already open, not a new transition"
        );
    }

    #[test]
    fn an_untouched_provider_is_never_open() {
        let circuits = ProviderCircuits::default();
        assert!(!circuits.is_open("anthropic", 0, &policy()));
        assert!(circuits.open_circuits(0, &policy()).is_empty());
    }

    #[test]
    fn a_success_closes_the_circuit() {
        let mut circuits = ProviderCircuits::default();
        for t in 0..3 {
            fail(&mut circuits, t);
        }
        assert!(circuits.is_open("openrouter", 2, &policy()));
        circuits.record_success("openrouter");
        assert!(!circuits.is_open("openrouter", 2, &policy()));
        // And the count restarts, so one later failure does not re-open it.
        assert!(!fail(&mut circuits, 10));
        assert!(!circuits.is_open("openrouter", 10, &policy()));
    }

    #[test]
    fn last_reason_reports_the_most_recent_failure_or_nothing() {
        let mut circuits = ProviderCircuits::default();
        assert_eq!(circuits.last_reason("p"), None);
        circuits.record_failure("p", UnavailableReason::CreditsExhausted, None, 0, &policy());
        assert_eq!(
            circuits.last_reason("p"),
            Some(UnavailableReason::CreditsExhausted),
            "one failure is enough for the reason, open or not"
        );
    }

    #[test]
    fn reset_forgets_every_circuit() {
        // What an explicit resume relies on: after a reset the next dispatch
        // is a real probe rather than a wait for the cooldown.
        let mut circuits = ProviderCircuits::default();
        let mut now = 0;
        while !fail(&mut circuits, now) {
            now += 1;
        }
        assert!(circuits.is_open("openrouter", now, &policy()));
        circuits.reset();
        assert!(!circuits.is_open("openrouter", now, &policy()));
        assert_eq!(circuits.last_reason("openrouter"), None);
    }

    #[test]
    fn the_cooldown_lets_a_probe_through() {
        let mut circuits = ProviderCircuits::default();
        for t in 0..3 {
            fail(&mut circuits, t);
        }
        assert!(circuits.is_open("openrouter", 2 + 299, &policy()));
        // Cooldown elapsed: the next dispatch is the probe.
        assert!(!circuits.is_open("openrouter", 2 + 300, &policy()));
    }

    #[test]
    fn a_failed_probe_restarts_the_cooldown() {
        let mut circuits = ProviderCircuits::default();
        for t in 0..3 {
            fail(&mut circuits, t);
        }
        // Probe at the end of the cooldown, and it fails again.
        assert!(
            !fail(&mut circuits, 302),
            "already open: not a new transition"
        );
        // The clock restarted from the probe rather than the original opening.
        assert!(circuits.is_open("openrouter", 400, &policy()));
        assert!(!circuits.is_open("openrouter", 602, &policy()));
    }

    #[test]
    fn a_zero_threshold_disables_the_breaker() {
        let disabled = CircuitPolicy {
            failures_before_open: 0,
            cooldown_secs: 300,
        };
        let mut circuits = ProviderCircuits::default();
        for t in 0..10 {
            assert!(!circuits.record_failure(
                "openrouter",
                UnavailableReason::CreditsExhausted,
                None,
                t,
                &disabled
            ));
        }
        assert!(!circuits.is_open("openrouter", 10, &disabled));
        assert!(circuits.open_circuits(10, &disabled).is_empty());
    }

    #[test]
    fn open_circuits_reports_what_the_operator_needs() {
        let mut circuits = ProviderCircuits::default();
        for t in 0..3 {
            fail(&mut circuits, t);
        }
        let open = circuits.open_circuits(102, &policy());
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].provider, "openrouter");
        assert_eq!(open[0].reason, UnavailableReason::CreditsExhausted);
        assert_eq!(open[0].consecutive_failures, 3);
        // Opened at t=2, cooldown 300, now 102 ⇒ 200 left.
        assert_eq!(open[0].retry_in_secs, 200);
    }

    #[test]
    fn open_circuits_is_sorted_and_drops_expired_ones() {
        let mut circuits = ProviderCircuits::default();
        for name in ["openrouter", "anthropic"] {
            for t in 0..3 {
                circuits.record_failure(name, UnavailableReason::AuthFailed, None, t, &policy());
            }
        }
        let open = circuits.open_circuits(10, &policy());
        assert_eq!(
            open.iter().map(|c| c.provider.as_str()).collect::<Vec<_>>(),
            vec!["anthropic", "openrouter"],
            "a HashMap's order is not stable; the report must be"
        );
        // Past the cooldown they are no longer open, so nothing is reported.
        assert!(circuits.open_circuits(1_000, &policy()).is_empty());
    }

    #[test]
    fn the_latest_reason_wins() {
        let mut circuits = ProviderCircuits::default();
        circuits.record_failure("p", UnavailableReason::CreditsExhausted, None, 0, &policy());
        circuits.record_failure("p", UnavailableReason::AuthFailed, None, 1, &policy());
        circuits.record_failure("p", UnavailableReason::AuthFailed, None, 2, &policy());
        let open = circuits.open_circuits(2, &policy());
        assert_eq!(open[0].reason, UnavailableReason::AuthFailed);
    }

    #[test]
    fn the_default_policy_is_three_strikes_and_five_minutes() {
        let p = CircuitPolicy::default();
        assert_eq!(p.failures_before_open, DEFAULT_FAILURES_BEFORE_OPEN);
        assert_eq!(p.cooldown_secs, DEFAULT_CIRCUIT_COOLDOWN_SECS);
    }

    // ── the rotation system ────────────────────────────────────────────────

    fn agent_state() -> AgentState {
        AgentState {
            agent_id: "a".to_string(),
            current_stage: "s".to_string(),
            iteration: 0,
            status: crate::components::AgentStatus::Active,
            spawned_children_ids: vec![],
            pending_wait: None,
            accepts_messages: true,
        }
    }

    fn stage_on(provider: &str, fallbacks: &[&str]) -> StageInference {
        StageInference {
            provider_name: provider.to_string(),
            model: format!("{provider}-model"),
            tools: Vec::new(),
            tool_filter: None,
            fallbacks: fallbacks
                .iter()
                .map(|p| {
                    leviath_core::blueprint::ModelEntry::new((*p).to_string(), format!("{p}-model"))
                })
                .collect(),
            output: None,
        }
    }

    /// A world with `open` providers already tripped.
    fn world_with_open(open: &[&str]) -> World {
        let mut world = World::new();
        let mut circuits = ProviderCircuits::default();
        let now = chrono::Utc::now().timestamp();
        for name in open {
            for _ in 0..policy().failures_before_open {
                circuits.record_failure(
                    name,
                    UnavailableReason::CreditsExhausted,
                    None,
                    now,
                    &policy(),
                );
            }
        }
        world.insert_resource(circuits);
        world.insert_resource(policy());
        world
    }

    fn run_rotate(world: &mut World) {
        let mut schedule = Schedule::default();
        schedule.add_systems(rotate_open_circuits);
        schedule.run(world);
    }

    #[test]
    fn rotation_moves_a_ready_agent_off_a_tripped_provider() {
        let mut world = world_with_open(&["openrouter"]);
        let e = world
            .spawn((
                agent_state(),
                super::ReadyToInfer,
                stage_on("openrouter", &["anthropic"]),
            ))
            .id();

        run_rotate(&mut world);

        let si = world.get::<StageInference>(e).unwrap();
        assert_eq!(si.provider_name, "anthropic");
        assert_eq!(si.model, "anthropic-model");
        assert!(si.fallbacks.is_empty());
    }

    #[test]
    fn rotation_skips_past_candidates_that_are_also_tripped() {
        let mut world = world_with_open(&["openrouter", "openai"]);
        let e = world
            .spawn((
                agent_state(),
                super::ReadyToInfer,
                stage_on("openrouter", &["openai", "anthropic"]),
            ))
            .id();

        run_rotate(&mut world);

        let si = world.get::<StageInference>(e).unwrap();
        assert_eq!(si.provider_name, "anthropic");
        // The tripped candidate is dropped rather than left to be tried next:
        // it is no better than what we just left.
        assert!(si.fallbacks.is_empty());
    }

    #[test]
    fn rotation_leaves_an_agent_with_nowhere_to_go_alone() {
        // Dispatch parks it on ProviderCircuitOpen; rotating to nothing would
        // just lose the provider name the operator needs to see.
        let mut world = world_with_open(&["openrouter"]);
        let e = world
            .spawn((
                agent_state(),
                super::ReadyToInfer,
                stage_on("openrouter", &[]),
            ))
            .id();

        run_rotate(&mut world);

        assert_eq!(
            world.get::<StageInference>(e).unwrap().provider_name,
            "openrouter"
        );
    }

    #[test]
    fn rotation_leaves_a_healthy_provider_alone() {
        let mut world = world_with_open(&["openrouter"]);
        let e = world
            .spawn((
                agent_state(),
                super::ReadyToInfer,
                stage_on("anthropic", &["openai"]),
            ))
            .id();

        run_rotate(&mut world);

        let si = world.get::<StageInference>(e).unwrap();
        assert_eq!(si.provider_name, "anthropic");
        assert_eq!(si.fallbacks.len(), 1, "no candidate was spent");
    }

    #[test]
    fn rotation_ignores_an_agent_that_is_not_active() {
        // A paused run must not have its provider changed underneath it.
        let mut world = world_with_open(&["openrouter"]);
        let mut state = agent_state();
        state.status = crate::components::AgentStatus::Paused;
        let e = world
            .spawn((
                state,
                super::ReadyToInfer,
                stage_on("openrouter", &["anthropic"]),
            ))
            .id();

        run_rotate(&mut world);

        assert_eq!(
            world.get::<StageInference>(e).unwrap().provider_name,
            "openrouter"
        );
    }

    #[test]
    fn rotation_is_a_no_op_without_the_breaker_installed() {
        // No `ProviderCircuits` resource, so the stage keeps the provider it
        // was given: an embedder that never inserts one gets no rotation.
        let mut world = World::new();
        let e = world
            .spawn((
                agent_state(),
                super::ReadyToInfer,
                stage_on("openrouter", &["anthropic"]),
            ))
            .id();

        run_rotate(&mut world);

        assert_eq!(
            world.get::<StageInference>(e).unwrap().provider_name,
            "openrouter"
        );
    }

    #[test]
    fn rotation_falls_back_to_the_default_policy() {
        // Circuits present, policy absent: the default must apply rather than
        // the breaker silently doing nothing.
        let mut world = World::new();
        let mut circuits = ProviderCircuits::default();
        let now = chrono::Utc::now().timestamp();
        let default_policy = CircuitPolicy::default();
        for _ in 0..default_policy.failures_before_open {
            circuits.record_failure(
                "openrouter",
                UnavailableReason::CreditsExhausted,
                None,
                now,
                &default_policy,
            );
        }
        world.insert_resource(circuits);
        let e = world
            .spawn((
                agent_state(),
                super::ReadyToInfer,
                stage_on("openrouter", &["anthropic"]),
            ))
            .id();

        run_rotate(&mut world);

        assert_eq!(
            world.get::<StageInference>(e).unwrap().provider_name,
            "anthropic"
        );
    }
}
