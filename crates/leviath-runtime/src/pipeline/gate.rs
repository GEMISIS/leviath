//! Taint-gate policy resources and the gate's block message.

use super::*;

/// Per-tool output sensitivity for an agent, populated by the taint-gate system
/// when taint tracking is enabled. Absent ⇒ taint off ⇒ results tagged Public.
#[derive(Component, Debug, Clone, Default)]
pub struct ToolSensitivities(pub std::collections::HashMap<String, leviath_core::TaintLevel>);

/// The tool allowlist policy (`policy.toml`), as a world resource. The daemon
/// inserts it; a taint-gated agent's outbound calls are checked against it. When
/// absent, the gate falls back to an empty policy (deny-by-clearance only).
#[derive(Resource, Default)]
pub struct PolicyGate(pub leviath_core::PolicyConfig);

/// The scripted gate rules (`~/.config/leviath/rules/*.rhai`), as a world
/// resource. The daemon builds the checker (it owns the Rhai engine); the gate
/// consults it after the static allowlist. Absent ⇒ no scripted rules.
#[derive(Resource, Clone)]
pub struct GateScriptRules(pub std::sync::Arc<crate::taint::ScriptRuleChecker>);

/// The `[blocked]` tool result produced when the taint gate denies an outbound
/// call: enough for the model to understand why and adjust.
pub(crate) fn taint_block_message(decision: &leviath_core::taint::GateDecision) -> String {
    match decision {
        leviath_core::taint::GateDecision::Blocked {
            taint_level,
            clearance,
            tool_name,
            source_regions,
        } => format!(
            "[blocked] Tool '{tool_name}' would send {taint_level:?}-level data over a channel \
             cleared only for {clearance:?} (tainted by: {}). Add an allowlist rule with \
             `lev policy add` to permit it.",
            if source_regions.is_empty() {
                "context".to_string()
            } else {
                source_regions.join(", ")
            }
        ),
        // Only ever called on a Blocked decision.
        leviath_core::taint::GateDecision::Allowed => "[blocked] tool call denied".to_string(),
    }
}
