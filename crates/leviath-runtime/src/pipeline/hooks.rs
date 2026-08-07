//! Running a stage's script hooks (issue #260).
//!
//! The contract lives in [`leviath_scripting::stage_hook`]; this module is the
//! pipeline half - when each hook fires, what the script is shown, and what
//! each outcome does to the agent.
//!
//! # Where `on_stage_enter` fires
//!
//! On the [`StageJustEntered`] marker the transition systems already set, and
//! **before** `sync_tool_stages`, which consumes it. That puts the hook after
//! the stage's layout and system prompt are in place (so it can read and write
//! real regions) and before the first inference of the stage is built (so what
//! it writes is in the request).
//!
//! Firing off a marker rather than inside `enter_stage` is deliberate:
//! `enter_stage` is a pure function called from three places, and threading the
//! compiled scripts plus an error channel through all of them would put script
//! execution in the middle of a transition. Here it is one system, one query,
//! and the failure modes stay at the tick boundary.

use super::*;
use crate::components::StageHookScripts;
use leviath_scripting::stage_hook::{HookOutcome, run};

/// The `ctx` a stage hook is shown.
///
/// Deliberately a snapshot rather than a handle: Rhai passes by value, so a
/// script could not mutate a live window even if it were given one, and
/// building the map is what makes the contract inspectable.
fn stage_ctx(stage_name: &str, index: usize, window: &ContextWindow) -> serde_json::Value {
    // Entries joined, not the entry list: a hook that wants to seed or rewrite a
    // region thinks in text, and handing it the internal entry shape would make
    // the ctx an implementation detail scripts then depend on.
    let regions: serde_json::Map<String, serde_json::Value> = window
        .regions
        .iter()
        .map(|r| {
            let text = r
                .content
                .iter()
                .map(|e| e.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            (r.name.clone(), serde_json::Value::String(text))
        })
        .collect();
    serde_json::json!({
        "stage": stage_name,
        "stage_index": index,
        "regions": regions,
    })
}

/// Apply a `modify` outcome: write each named region's new content.
///
/// A name the window does not have is reported rather than ignored. The script
/// asked to write somewhere that does not exist, which is a bug in the script
/// or a stale region name in the blueprint - and silently dropping it would
/// look exactly like a hook that ran and chose to write nothing.
fn apply_modify(window: &mut ContextWindow, value: &serde_json::Value) -> Result<(), String> {
    let Some(obj) = value.as_object() else {
        return Err(format!(
            "on_stage_enter: 'value' must be a map of region name to content, got: {value}"
        ));
    };
    for (name, content) in obj {
        let Some(text) = content.as_str() else {
            return Err(format!(
                "on_stage_enter: region '{name}' must be given a string, got: {content}"
            ));
        };
        let Some(region) = window.get_region_mut(name) else {
            return Err(format!(
                "on_stage_enter: no region '{name}' in this stage's layout"
            ));
        };
        // Replace, not append: the hook was shown the region's whole text and
        // returned what it should be. Appending would make a hook that echoes
        // its input double the region every time the stage is re-entered.
        region.clear();
        if !text.is_empty() {
            region
                .add_entry(text.to_string(), leviath_core::estimate_tokens(text))
                .map_err(|e| format!("on_stage_enter: writing region '{name}': {e}"))?;
        }
    }
    Ok(())
}

/// Run every entering agent's `on_stage_enter` hook.
///
/// Ordered before `sync_tool_stages` (which clears [`StageJustEntered`]) and
/// therefore before the stage's first inference is built.
pub fn run_stage_enter_hooks(
    mut agents: Query<(
        Entity,
        &StageJustEntered,
        &AgentBlueprint,
        &StageHookScripts,
        &mut ContextWindow,
        &mut AgentState,
    )>,
) {
    crate::tick_scope::clear();
    for (entity, entered, bp, scripts, mut window, mut state) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        let Some(stage) = bp.0.stages.get(entered.index) else {
            continue;
        };
        let Some(script) = scripts.script_for(stage, "on_stage_enter") else {
            continue;
        };

        let ctx = stage_ctx(&entered.name, entered.index, &window);
        let outcome = match run(&script, "on_stage_enter", ctx) {
            Ok(o) => o,
            Err(e) => {
                // A hook that fails is not a hook that allowed. Failing the run
                // is the same stance `enter_stage` takes on a prompt that will
                // not fit: the stage cannot start as configured.
                state.status = AgentStatus::Error {
                    message: format!("on_stage_enter hook failed: {e}"),
                };
                continue;
            }
        };

        match outcome {
            HookOutcome::Allow => {}
            HookOutcome::Modify(value) => {
                if let Err(e) = apply_modify(&mut window, &value) {
                    state.status = AgentStatus::Error { message: e };
                }
            }
            HookOutcome::Cancel(reason) => {
                let why = reason.unwrap_or_else(|| "no reason given".to_string());
                state.status = AgentStatus::Error {
                    message: format!("on_stage_enter refused stage '{}': {why}", entered.name),
                };
            }
            // Retrying entry into a stage the agent is already in has no
            // meaning. Saying so beats treating it as `Allow`, which would let
            // a script think it had asked for something.
            HookOutcome::Retry => {
                state.status = AgentStatus::Error {
                    message: format!(
                        "on_stage_enter returned 'retry', which this hook cannot honour \
                         (stage '{}' is already entered)",
                        entered.name
                    ),
                };
            }
        }
    }
}

/// Set the agent's status from a hook outcome the caller could not honour.
///
/// Shared by the hooks below so "a failed hook is not an allowed hook" is
/// written once rather than restated per call site with a chance to drift.
fn refuse(state: &mut AgentState, hook: &str, what: String) {
    state.status = AgentStatus::Error {
        message: format!("{hook}: {what}"),
    };
}

/// Run `before_inference` for every agent about to infer.
///
/// Scheduled before `dispatch_inference`, on the same `ReadyToInfer` marker it
/// queries. The context window is assembled by then, so `modify` here reaches
/// the request: `build_request` reads the window fresh at dispatch.
///
/// Fired from its own system rather than inside `dispatch_inference` because
/// that system's per-agent body runs in parallel on the compute pool, where a
/// panicking script would be attributed by different machinery. Sequential here
/// costs a query pass and keeps script failures at the tick boundary.
#[allow(clippy::type_complexity)]
pub fn run_before_inference_hooks(
    mut agents: Query<
        (
            Entity,
            &StageCursor,
            &AgentBlueprint,
            &StageHookScripts,
            &mut ContextWindow,
            &mut AgentState,
        ),
        With<ReadyToInfer>,
    >,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, cursor, bp, scripts, mut window, mut state) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        let Some(stage) = bp.0.stages.get(cursor.index) else {
            continue;
        };
        let Some(script) = scripts.script_for(stage, "before_inference") else {
            continue;
        };

        let ctx = stage_ctx(&stage.name, cursor.index, &window);
        match run(&script, "before_inference", ctx) {
            Err(e) => refuse(&mut state, "before_inference", format!("hook failed: {e}")),
            Ok(HookOutcome::Allow) => {}
            Ok(HookOutcome::Modify(value)) => {
                if let Err(e) = apply_modify(&mut window, &value) {
                    refuse(&mut state, "before_inference", e);
                }
            }
            // Skipping the call is what `cancel` means here, and the agent
            // stops rather than silently inferring anyway. `ReadyToInfer` is
            // removed so `dispatch_inference` does not pick it up this tick.
            Ok(HookOutcome::Cancel(reason)) => {
                let why = reason.unwrap_or_else(|| "no reason given".to_string());
                refuse(
                    &mut state,
                    "before_inference",
                    format!("refused the inference: {why}"),
                );
                commands.entity(entity).remove::<ReadyToInfer>();
            }
            // Nothing has happened yet, so there is nothing to do again.
            Ok(HookOutcome::Retry) => refuse(
                &mut state,
                "before_inference",
                "returned 'retry', which this hook cannot honour (nothing has run yet)".to_string(),
            ),
        }
    }
}

/// Run `after_inference` with the model's response in hand.
///
/// Scheduled before `process_response`, on the `ProcessResponse` marker, so the
/// hook sees the response before anything is written to context or any tool
/// call is dispatched from it.
///
/// `modify` replaces the response **text**. It deliberately cannot rewrite the
/// tool calls: those are about to be checked by the policy and taint layers, and
/// a hook that could rewrite them would be a way around checks the operator
/// configured. Steering tool calls is `on_tool_call`'s job, where the gate can
/// see it.
#[allow(clippy::type_complexity)]
pub fn run_after_inference_hooks(
    mut agents: Query<
        (
            Entity,
            &StageCursor,
            &AgentBlueprint,
            &StageHookScripts,
            &mut crate::components::InferenceResult,
            &mut AgentState,
        ),
        With<ProcessResponse>,
    >,
) {
    crate::tick_scope::clear();
    for (entity, cursor, bp, scripts, mut result, mut state) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        let Some(stage) = bp.0.stages.get(cursor.index) else {
            continue;
        };
        let Some(script) = scripts.script_for(stage, "after_inference") else {
            continue;
        };

        let ctx = serde_json::json!({
            "stage": stage.name,
            "stage_index": cursor.index,
            "response": result.response,
            "tokens_used": result.tokens_used,
            // Names only: enough for a hook to notice "it wants to run shell"
            // without implying it can rewrite the call.
            "tool_calls": result
                .tool_calls
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>(),
        });

        match run(&script, "after_inference", ctx) {
            Err(e) => refuse(&mut state, "after_inference", format!("hook failed: {e}")),
            Ok(HookOutcome::Allow) => {}
            Ok(HookOutcome::Modify(value)) => match value.as_str() {
                Some(text) => result.response = text.to_string(),
                None => refuse(
                    &mut state,
                    "after_inference",
                    format!("'value' must be the replacement response text, got: {value}"),
                ),
            },
            Ok(HookOutcome::Cancel(reason)) => {
                let why = reason.unwrap_or_else(|| "no reason given".to_string());
                refuse(
                    &mut state,
                    "after_inference",
                    format!("rejected the response: {why}"),
                );
            }
            // Re-inferring is a real thing to want here (a malformed answer),
            // but it needs the request rebuilt and the attempt counted, or a
            // hook that always retries wedges the run. Refused explicitly until
            // that is built, rather than silently ignored.
            Ok(HookOutcome::Retry) => refuse(
                &mut state,
                "after_inference",
                "returned 'retry', which is not implemented yet - re-inference needs an \
                 attempt bound so a hook cannot wedge the run"
                    .to_string(),
            ),
        }
    }
}
