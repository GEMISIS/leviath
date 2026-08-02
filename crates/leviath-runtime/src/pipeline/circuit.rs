//! Per-provider circuit breakers: stop hammering a provider that has told us,
//! repeatedly, that it cannot serve anyone.
//!
//! Failing over (see [`super::response::collect_inference`]) rescues one run.
//! It does nothing for the *next* run, which starts on the same dead provider
//! and burns its own failure discovering the same thing. Issue #201 is what
//! that looks like at scale: ten consecutive workers, every one of them dying
//! at iteration 0 against an OpenRouter account with no credits left.
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

use leviath_providers::UnavailableReason;
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
pub struct Circuit {
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

impl ProviderCircuits {
    /// Count a provider-fatal failure against `provider`.
    ///
    /// Returns `true` on the transition into the open state, so the caller can
    /// log and alert exactly once rather than on every subsequent failure.
    pub fn record_failure(
        &mut self,
        provider: &str,
        reason: UnavailableReason,
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
        if policy.failures_before_open == 0 {
            return false; // breaker disabled; keep counting for the record
        }
        let was_open = entry.opened_at.is_some();
        if entry.consecutive_failures >= policy.failures_before_open {
            // Re-stamp on every failure at or past the threshold: a probe that
            // fails must restart the cooldown, not inherit the old one.
            entry.opened_at = Some(now);
        }
        !was_open && entry.opened_at.is_some()
    }

    /// Forget `provider`'s failures. Any success proves it is serving again.
    pub fn record_success(&mut self, provider: &str) {
        self.0.remove(provider);
    }

    /// Whether `provider` should be skipped right now.
    ///
    /// False once the cooldown has elapsed, which is what makes the next
    /// request a probe without needing a distinct half-open state.
    pub fn is_open(&self, provider: &str, now: i64, policy: &CircuitPolicy) -> bool {
        self.0
            .get(provider)
            .and_then(|c| c.opened_at)
            .is_some_and(|at| now.saturating_sub(at) < policy.cooldown_secs as i64)
    }

    /// Every currently-open circuit, provider-sorted so the rendering is
    /// stable across ticks (a `HashMap` iteration order is not).
    pub fn open_circuits(&self, now: i64, policy: &CircuitPolicy) -> Vec<ProviderCircuitState> {
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
pub fn rotate_open_circuits(
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
            now,
            &policy(),
        )
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
                circuits.record_failure(name, UnavailableReason::AuthFailed, t, &policy());
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
        circuits.record_failure("p", UnavailableReason::CreditsExhausted, 0, &policy());
        circuits.record_failure("p", UnavailableReason::AuthFailed, 1, &policy());
        circuits.record_failure("p", UnavailableReason::AuthFailed, 2, &policy());
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
        }
    }

    /// A world with `open` providers already tripped.
    fn world_with_open(open: &[&str]) -> World {
        let mut world = World::new();
        let mut circuits = ProviderCircuits::default();
        let now = chrono::Utc::now().timestamp();
        for name in open {
            for _ in 0..policy().failures_before_open {
                circuits.record_failure(name, UnavailableReason::CreditsExhausted, now, &policy());
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
        // An embedder that never inserts the resource keeps the old behavior.
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
