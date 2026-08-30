//! The wedge watchdog: fail a run that no system can ever look at again.
//!
//! Every non-terminal agent rests between ticks holding exactly one phase
//! marker, and each marker is a claim some system has on it: `ReadyToInfer` is
//! dispatch's, `AwaitingInference` is the inference collector's, `FanOutWaiting`
//! is the fan-out collector's, and so on. The marker is what makes the agent
//! reachable. An agent that is non-terminal and holds *none* of them is in a
//! state no query matches, so nothing will ever touch it again, and it stays
//! `running` in `meta.json` for the life of the daemon.
//!
//! That is not hypothetical. `PipelineWorld` already logs "a pipeline system
//! panicked outside any agent's scope; the daemon survived (an agent may be
//! wedged - cancel it via `lev cancel <run-id>`)": the runtime knows it can
//! strand a run and asks a person to clean up. When the "person" is an
//! unattended harness nobody ever does: the run occupies a slot for ever, and a
//! factory runs down to zero free slots over a few hours.
//!
//! ## Why this cannot produce a false positive
//!
//! The trigger is structural, not temporal. It is not "this run looks old" -
//! that is the shape of every misdiagnosis in this area, where a fresh
//! `updated_at`, a bare `waiting`, or a `pid` field gets read as evidence it
//! was never able to give. It is "the pipeline's own
//! invariants say this state cannot exist", and the timeout only absorbs the
//! transient windows inside a tick.
//!
//! Those invariants hold on both sides. Every site that removes a phase marker
//! either inserts a successor or sets a terminal status, and `spawn_agent_seeded`
//! always lands `Active + ReadyToInfer`, so no ordinary path arrives here.
//!
//! What is *not* touched, and why:
//!
//! - A long inference holds `AwaitingInference` and `InFlightWork`, and is
//!   bounded twice over besides (the provider's job timeout, and the lane
//!   supervisor that turns a dead task into an ordinary error outcome). A
//!   fifteen-minute call is never a candidate.
//! - A full inference pool leaves the agent `ReadyToInfer` with a
//!   [`DispatchStall`](super::DispatchStall). That is backpressure working as
//!   designed, and the stall watchdog already declines to fail it.
//! - A tool batch holds `AwaitingTools` and is deliberately unbounded: it may
//!   park off-lane on a tool approval, an `ask_user`, or a `wait_for_agent` that
//!   ends only when some other run does. Any clock-based rule would have to
//!   guess a bound here. This one does not have to.
//! - A run blocked on a person holds an interaction marker and is left entirely
//!   alone, and deliberately so: killing a run somebody is about to answer is a
//!   worse failure than leaking a slot, and two timeouts racing over one status
//!   is how a run's status ends up lying about what happened to it.
//! - `Paused` is skipped before the clock is even read, and loses any record it
//!   was carrying, so resuming never finds a run the watchdog had started
//!   counting.
//!
//! One residual class this does not cover, stated plainly: an agent that *holds*
//! a marker but is missing some component the matching system's query also
//! requires, so that query never sees it. No production path builds an agent
//! that way today. Covering it would need exactly the "looks old" reasoning the
//! rest of this module exists to avoid, so it is left uncovered rather than
//! guessed at.
//!
//! ## It composes upward
//!
//! Failing a wedged child is enough to free its parent. The fan-out collector
//! already counts an `Error` worker as finished, and a `requires_children` gate
//! releases on any terminal child, so there is no parent case to special-case.

use super::*;

/// An agent found in a state no system can reach, and when it was first seen
/// that way.
///
/// One field, unlike [`DispatchStall`](super::DispatchStall), which also carries
/// a freshness stamp. That record is written by the dispatch systems and read by
/// a different one, so it has to cope with its writer going away. This one has a
/// single owner: the watchdog inserts it when the condition holds, keeps the
/// original `since` while it keeps holding, and removes it the moment the agent
/// becomes reachable again. There is nothing to go stale.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Wedged {
    /// Unix seconds when the agent was first seen unreachable.
    pub since: i64,
}

/// How long an agent may sit unreachable before the run is failed.
///
/// A world resource rather than a constant because the daemon serves it from
/// `[limits] wedge_timeout_secs`. Zero disables the watchdog.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct WedgeTimeout(pub u64);

impl Default for WedgeTimeout {
    fn default() -> Self {
        Self(DEFAULT_WEDGE_TIMEOUT_SECS)
    }
}

/// Default grace period before an unreachable agent fails its run: `0`, meaning
/// the watchdog is off unless an operator turns it on.
///
/// Off by default because this fails runs, and a daemon that starts killing work
/// after an upgrade nobody asked for is a worse outcome than the leak it
/// prevents. `300` is the value to set once you want it: five minutes is ten of
/// the daemon's thirty-second re-drives, the same span `dead_cycles_before_relief`
/// already treats as "long enough to act on".
pub const DEFAULT_WEDGE_TIMEOUT_SECS: u64 = 0;

/// Query filter: the agent holds nothing that will cause any system to look at
/// it again.
///
/// Every entry is a component whose *presence* means some system has this agent
/// queued. This is the load-bearing list in the module, and the one thing here
/// that needs maintaining: **a new phase marker must be added here**, or an agent
/// resting on it will be mistaken for an unreachable one. The table-driven test
/// at the bottom of this file is what catches that.
///
/// Note that [`PipelineWorld::fingerprint`](crate::world::PipelineWorld) counts a
/// subset of these markers for a different purpose (deciding whether a tick
/// changed anything). The two lists answer different questions and are not
/// interchangeable.
///
/// Components that are per-agent *data* rather than a claim on the agent -
/// `StageCursor`, `StageProgress`, `DynamicTools`, the auto-approve markers, and
/// `DispatchStall` itself - are deliberately absent. Their presence says nothing
/// about whether anything is going to run.
pub(crate) type Unreachable = (
    (
        Without<ReadyToInfer>,
        Without<AwaitingInference>,
        Without<ProcessResponse>,
        Without<ReadyForTools>,
        Without<AwaitingTools>,
        Without<ReadyForTransition>,
        Without<ResolveTransition>,
        Without<ToolsNeedRefresh>,
        Without<StageJustEntered>,
        Without<AwaitingTransitionChoice>,
        Without<AwaitingTransitionResponse>,
        Without<WaitingForChildren>,
    ),
    (
        Without<AwaitingCompaction>,
        Without<PendingEdgeCompact>,
        Without<crate::context_transform::AwaitingContentSummary>,
        Without<crate::context_transform::PendingContentSummary>,
        Without<crate::title::PendingTitle>,
        Without<crate::title::AwaitingTitle>,
        Without<crate::components::AwaitingInteraction>,
        Without<crate::gate_prompt::AwaitingGatePrompt>,
        Without<crate::gate_prompt::GateResolved>,
        Without<crate::interaction_points::ReadyForInteractionPoint>,
        Without<crate::interaction_points::AwaitingInteractionPoint>,
        Without<crate::fanout::FanOutWaiting>,
    ),
    (
        Without<InFlightWork>,
        Without<crate::tick_scope::PanickedInParallel>,
    ),
);

/// What `fail_wedged_runs` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type WedgedRunQuery = (
    Entity,
    Option<&'static Wedged>,
    &'static mut AgentState,
    Option<&'static mut StageIoBuffer>,
);

/// Wedge watchdog: fail any non-terminal agent that has been unreachable for
/// longer than [`WedgeTimeout`].
///
/// See the module documentation for why this is safe. In short: an agent matches
/// [`Unreachable`] only in a state the rest of the pipeline guarantees it never
/// leaves an agent in, so anything that matches is already lost.
pub(crate) fn fail_wedged_runs(
    mut agents: Query<WedgedRunQuery, Unreachable>,
    timeout: Option<Res<WedgeTimeout>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    let limit = timeout.map(|t| t.0).unwrap_or(DEFAULT_WEDGE_TIMEOUT_SECS);
    if limit == 0 {
        return; // watchdog disabled
    }
    let now = chrono::Utc::now().timestamp();
    for (entity, wedged, mut state, buffer) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        // A terminal agent is the goal, not a problem, and the host reaps it. A
        // paused one is stopped because somebody stopped it, and must come back
        // with no clock already running against it.
        if super::is_terminal_status(&state.status) || state.status == AgentStatus::Paused {
            if wedged.is_some() {
                commands.entity(entity).remove::<Wedged>();
            }
            continue;
        }
        let since = wedged.map(|w| w.since).unwrap_or(now);
        if now.saturating_sub(since) < limit as i64 {
            // Inside the grace period: record it (keeping the original start) so
            // the next tick measures the whole wait rather than restarting.
            commands.entity(entity).insert(Wedged { since });
            continue;
        }
        let waited = now.saturating_sub(since);
        let message = format!(
            "run stopped being driven in stage '{}': {waited}s with nothing in flight and \
             no marker any system acts on, so it can never move again. Failing it releases \
             the capacity it was holding. This is a bug in Leviath - please report it with \
             the daemon log",
            state.current_stage
        );
        tracing::error!(
            stage = %state.current_stage,
            wedged_secs = waited,
            "failing a run that nothing can drive"
        );
        if let Some(mut buffer) = buffer {
            buffer.logs.push((0, format!("[wedged] {message}")));
        }
        commands.entity(entity).remove::<Wedged>();
        // Routing it is also what un-wedges it: `ResolveTransition` is a marker a
        // system acts on, so a run whose blueprint declares an `error` edge gets
        // driven into that stage instead of ending here.
        crate::pipeline::fail_stage(&mut commands, entity, &mut state, message);
    }
}

#[cfg(test)]
mod tests;
