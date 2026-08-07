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
