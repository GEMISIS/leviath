//! Whether a chosen edge's gate blocks the transition, and the attempt
//! accounting behind a re-run. Moved out of `transition.rs` whole; nothing
//! here changed but the file it lives in.

use super::*;

/// Decide whether a chosen edge's [gate](leviath_core::blueprint::TransitionGate)
/// blocks the transition.
///
/// The failure this guards against: an agent can read and reason about a
/// codebase entirely through `shell` and arrive at the review stage having
/// changed nothing, producing a run
/// with no output. A `require_modifications` gate keeps it in the stage until it
/// has actually written something.
///
/// The gate passes when any of these hold:
/// - the stage advertises no file-modifying tool (it could never pass, so gating
///   it would only burn iterations);
/// - a modifying tool call succeeded in this stage;
/// - one was refused by the permission layer (the agent is trying and cannot);
/// - the gate names a region and that region is non-empty (the durable signal:
///   per-stage counters don't survive a daemon restart, but regions do).
///
/// When the gate's re-run budget is spent it gives up loudly, as
/// [`GateDecision::Forced`].
pub(crate) fn gate_blocks(
    gate: Option<&leviath_core::blueprint::TransitionGate>,
    stage: &leviath_core::Stage,
    progress: &StageProgress,
    window: &ContextWindow,
) -> GateDecision {
    let Some(gate) = gate else {
        return GateDecision::Pass;
    };
    // Checked before `require_modifications` and independently of it: an edge
    // may ask for a changed region without asking for a file write, and a
    // revise loop usually does exactly that.
    //
    // A missing baseline means the gate names a region the window does not
    // hold. A gate cannot demand an update to something that does not exist,
    // and blocking on it would strand the run, so it passes.
    if let Some(name) = &gate.require_region_updated
        && let (Some(before), Some(region)) = (
            progress.entry_region_digests.get(name),
            window.get_region(name),
        )
        && *before == region_digest(region)
    {
        return spend_gate_attempt(
            gate,
            stage,
            progress,
            gate.message.clone().unwrap_or_else(|| {
                format!(
                    "The `{name}` region is unchanged since this stage began. Whatever sent \
                     you back here was not answered by repeating the same content - revise it \
                     before moving on."
                )
            }),
        );
    }
    // Checked before `require_modifications` and independently of it: a stage
    // whose work is a set of items usually has no file write to require.
    //
    // A gate naming a region the window does not hold passes rather than
    // blocking - no amount of work could satisfy it, and stranding the run over
    // a typo in a region name would be worse than the missing check.
    if let Some(name) = &gate.require_no_open_items
        && let Some(region) = window.get_region(name)
    {
        let open = region.open_checklist_items();
        if !open.is_empty() {
            let cap = gate
                .max_attempts
                .unwrap_or(leviath_core::blueprint::DEFAULT_GATE_ATTEMPTS);
            if progress.gate_reentries >= cap {
                tracing::warn!(
                    stage = %stage.name,
                    open = open.len(),
                    attempts = cap,
                    "stage still has open checklist items after re-run attempts; proceeding"
                );
                return GateDecision::Forced;
            }
            let listed = open
                .iter()
                .map(|i| format!("{} {}", i.id, i.text))
                .collect::<Vec<_>>()
                .join("; ");
            return GateDecision::Block(gate.message.clone().unwrap_or_else(|| {
                format!(
                    "{} item(s) are still open in `{name}`: {listed}. Finish them, or use \
                     todo_done to drop the ones that no longer apply, before moving on.",
                    open.len()
                )
            }));
        }
    }
    // Conjunctive, and checked before `require_modifications` so it holds
    // whatever else the gate asks for. `gate.region` below is one of several
    // *alternative* ways to satisfy `require_modifications`, which is why it
    // cannot express "do not leave without writing this".
    let missing: Vec<&str> = gate
        .require_regions
        .iter()
        .filter(|name| {
            match window.get_region(name) {
                Some(region) => region.content.is_empty(),
                // Not held by the window at all. `lev validate` refuses a gate
                // naming a region no stage declares, so this means a layout
                // moved underneath the edge; blocking would strand the run over
                // something no amount of work could satisfy.
                None => {
                    tracing::warn!(
                        stage = %stage.name,
                        region = %name,
                        "gate requires a region this stage's window does not hold; \
                         letting the transition through"
                    );
                    false
                }
            }
        })
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        let listed = missing.join(", ");
        return spend_gate_attempt(
            gate,
            stage,
            progress,
            gate.message.clone().unwrap_or_else(|| {
                format!(
                    "This stage is not finished: the `{listed}` region is still empty. \
                     Write it with context_write before moving on - later stages read \
                     from it, and there is nothing there yet."
                )
            }),
        );
    }
    if !gate.require_modifications {
        return GateDecision::Pass;
    }
    let can_modify = stage.available_tools.iter().any(|t| {
        let canonical = leviath_tools::canonical_tool_name(t);
        leviath_core::blueprint::MODIFYING_TOOLS.contains(&canonical)
            || gate
                .tools
                .iter()
                .any(|extra| leviath_tools::canonical_tool_name(extra) == canonical)
    });
    if !can_modify {
        return GateDecision::Pass;
    }
    if progress.modifying_tool_calls > 0 {
        return GateDecision::Pass;
    }
    if progress.blocked_modification_calls > 0 {
        tracing::warn!(
            stage = %stage.name,
            blocked = progress.blocked_modification_calls,
            "file modifications were denied by policy; letting the gated transition through"
        );
        return GateDecision::Pass;
    }
    if let Some(region) = &gate.region
        && window
            .get_region(region)
            .is_some_and(|r| !r.content.is_empty())
    {
        return GateDecision::Pass;
    }
    spend_gate_attempt(
        gate,
        stage,
        progress,
        gate.message.clone().unwrap_or_else(|| {
            "No file modifications were recorded in this stage. Changes made through the shell \
             (sed -i, tee, >, >>) are not tracked by the framework. Re-apply your changes with \
             edit_file or write_file before moving on."
                .to_string()
        }),
    )
}

/// Block with `nudge`, or give up and let the edge through once the gate's
/// re-run budget is spent.
///
/// Shared by every gate condition so one blueprint key (`max_attempts`) bounds
/// all of them: a gate that could block forever would strand the run, which is
/// worse than letting a questionable transition through with a warning.
fn spend_gate_attempt(
    gate: &leviath_core::blueprint::TransitionGate,
    stage: &leviath_core::Stage,
    progress: &StageProgress,
    nudge: String,
) -> GateDecision {
    let cap = gate
        .max_attempts
        .unwrap_or(leviath_core::blueprint::DEFAULT_GATE_ATTEMPTS);
    if progress.gate_reentries >= cap {
        tracing::warn!(
            stage = %stage.name,
            attempts = cap,
            "transition gate still unsatisfied after re-run attempts; proceeding"
        );
        return GateDecision::Forced;
    }
    GateDecision::Block(nudge)
}
