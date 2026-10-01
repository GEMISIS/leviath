//! Parsing a stage's `[stages.<name>.transitions.<target>]` edges: their
//! conditions, gates, stuck rules and context transforms, and the nudge.

use super::*;

/// Parse `[stages.<name>.transitions.<target>]` into the stage's edge map.
///
/// Unknown conditions and transforms are rejected here rather than degraded,
/// because both failure modes build an edge the runtime never takes and a
/// dead edge is invisible until the run wedges.
pub(super) fn parse_transitions(
    stage: &str,
    transitions_table: &toml::value::Table,
) -> Result<std::collections::HashMap<String, TransitionEdge>> {
    let mut transitions = std::collections::HashMap::new();
    for (target_name, edge_value) in transitions_table {
        let where_ = format!("{stage}: transition to '{target_name}'");
        if let Some(edge_table) = edge_value.as_table() {
            reject_unknown_keys(
                &format!("transition to '{target_name}'"),
                edge_table,
                EDGE_KEYS,
            )?;
            if let Some(gate_table) = table_of(edge_table, "gate") {
                reject_unknown_keys(
                    &format!("transition to '{target_name}': gate"),
                    gate_table,
                    GATE_KEYS,
                )?;
            }
        }

        let hint = str_of(edge_value, "hint").map(|s| s.to_string());

        let condition = match str_of(edge_value, "condition") {
            Some("error") => TransitionCondition::Error,
            Some("max_iterations") => TransitionCondition::MaxIterations,
            Some("llm_choice") => TransitionCondition::LlmChoice,
            Some("stuck") => TransitionCondition::Stuck,
            Some("dead_end") => TransitionCondition::DeadEnd,
            Some("always") | None => TransitionCondition::Always,
            // Reject unknown conditions rather than silently building a
            // `Custom(..)` edge the runtime never evaluates (a dead edge).
            Some(other) => {
                return Err(Error::Other(format!(
                    "transition to '{target_name}' has unknown condition \
                     '{other}' (valid: always, error, max_iterations, \
                     llm_choice, stuck, dead_end)"
                )));
            }
        };

        // Stuck thresholds live on the edge they arm, so a stage can
        // be armed on iterations while another is armed on wall clock.
        // Both halves are required together: a bare `condition =
        // "stuck"` edge could never fire, and thresholds under any
        // other condition would be silently ignored.
        let stuck = parse_stuck_config(&where_, edge_value)?;
        let is_stuck = condition == TransitionCondition::Stuck;
        if is_stuck && stuck.is_none() {
            return Err(Error::Other(format!(
                "transition to '{target_name}' has condition 'stuck' but no \
                 threshold (set at least one of stuck_after_iterations, \
                 stuck_after_minutes, stuck_after_same_file_edits, \
                 stuck_after_tool_calls)"
            )));
        }
        if !is_stuck && stuck.is_some() {
            return Err(Error::Other(format!(
                "transition to '{target_name}' sets stuck_after_* thresholds \
                 but its condition is not 'stuck' - they would never be read"
            )));
        }

        let transform = match str_of(edge_value, "transform") {
            Some("clear") => EdgeTransform::Clear,
            Some("compact") | Some("summarize") => EdgeTransform::Compact { prompt: None },
            Some("custom") => {
                // Parse transform_config sub-table
                let tc = edge_value.get("transform_config");
                let carry = tc
                    .and_then(|v| v.get("carry"))
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let compact = tc
                    .and_then(|v| v.get("compact"))
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let clear = tc
                    .and_then(|v| v.get("clear"))
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                let compact_prompt = tc
                    .and_then(|v| v.get("compact_prompt"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                EdgeTransform::Custom {
                    carry,
                    compact,
                    clear,
                    compact_prompt,
                }
            }
            Some("direct") | None => EdgeTransform::Direct,
            // Reject unknown transforms rather than silently downgrading
            // to a plain `Direct` copy (a typo would pass unnoticed).
            Some(other) => {
                return Err(Error::Other(format!(
                    "transition to '{target_name}' has unknown transform \
                     '{other}' (valid: direct, clear, compact, summarize, custom)"
                )));
            }
        };

        // Parse the edge gate: `gate = { require_modifications = true, ... }`
        // (or a `[stages.<name>.transitions.<target>.gate]` sub-table).
        let gate = match table_of(edge_value, "gate") {
            Some(table) => Some(parse_transition_gate(&format!("{where_}: gate"), table)?),
            None => None,
        };

        transitions.insert(
            target_name.clone(),
            TransitionEdge {
                target: target_name.clone(),
                condition,
                hint,
                transform,
                gate,
                stuck,
            },
        );
    }
    Ok(transitions)
}

/// Parse a `[security]` / `[stages.X.security]` table into a `SecurityConfig`.
/// A present block defaults `taint_tracking` to `true` (block presence implies
/// intent to configure security); omit the block entirely to inherit the
/// broader (agent/global) setting.
/// Parse a transition edge's `gate = { ... }` table. Every key is optional; an
/// empty table yields a gate that blocks nothing (`require_modifications` off).
pub(super) fn parse_transition_gate(
    where_: &str,
    table: &toml::value::Table,
) -> Result<leviath_runtime::spec::blueprint::TransitionGate> {
    let mut gate = leviath_runtime::spec::blueprint::TransitionGate::default();
    if let Some(rm) = bool_of(table, "require_modifications") {
        gate.require_modifications = rm;
    }
    if let Some(msg) = str_of(table, "message") {
        gate.message = Some(msg.trim().to_string());
    }
    if let Some(region) = str_of(table, "region") {
        gate.region = Some(region.to_string());
    }
    if let Some(region) = str_of(table, "require_region_updated") {
        gate.require_region_updated = Some(region.to_string());
    }
    if let Some(regions) = array_of(table, "require_regions") {
        gate.require_regions = regions
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
    }
    if let Some(region) = str_of(table, "require_no_open_items") {
        gate.require_no_open_items = Some(region.to_string());
    }
    // An inline table, `{ region = "views", at_least = 4 }`. Both halves are
    // required and the count must be positive: a count of zero is a gate that
    // passes every time, which is a typo, not a wish.
    if let Some(count) = table_of(table, "require_region_entries") {
        let Some(region) = str_of(count, "region") else {
            return Err(Error::Other(format!(
                "{where_}: require_region_entries needs a `region`"
            )));
        };
        let at_least = count_of(count, where_, "at_least")?.unwrap_or(0);
        if at_least == 0 {
            return Err(Error::Other(format!(
                "{where_}: require_region_entries needs `at_least` of 1 or more"
            )));
        }
        gate.require_region_entries = Some(leviath_runtime::spec::blueprint::RegionCount {
            region: region.to_string(),
            at_least,
        });
    }
    if let Some(tools) = array_of(table, "tools") {
        gate.tools = tools
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
    }
    // A negative budget is a typo, not "never hold the stage", so it is
    // refused rather than falling back to the default without a word.
    if let Some(max) = count_of(table, where_, "max_attempts")? {
        gate.max_attempts = Some(max);
    }
    Ok(gate)
}

/// Parse a transition edge's `stuck_after_*` thresholds into a [`StuckConfig`],
/// or `None` when the edge arms none of them.
///
/// Zero reads as unset - mirroring `enforce_max_iterations`, where `max == 0`
/// means "unlimited" - so `stuck_after_iterations = 0` leaves the edge unarmed
/// and the caller rejects it, rather than the edge firing on turn zero. A
/// negative is refused rather than read as unset.
pub(super) fn parse_stuck_config(where_: &str, edge: &toml::Value) -> Result<Option<StuckConfig>> {
    let threshold = |key: &str| count_of(edge, where_, key).map(|n| n.filter(|n| *n > 0));
    let cfg = StuckConfig {
        after_iterations: threshold("stuck_after_iterations")?,
        after_minutes: threshold("stuck_after_minutes")?,
        after_same_file_edits: threshold("stuck_after_same_file_edits")?,
        after_tool_calls: threshold("stuck_after_tool_calls")?,
    };
    Ok(cfg.is_armed().then_some(cfg))
}

/// Parse one `[[transforms]]` entry: a parent region mapped onto a child region
/// when a sub-agent is spawned, optionally transformed en route.
pub(super) fn parse_context_transform(t: &toml::Value) -> ContextTransform {
    ContextTransform {
        from_blueprint: str_field(t, "from_blueprint"),
        to_blueprint: str_field(t, "to_blueprint"),
        mappings: array_of(t, "mappings")
            .map(|arr| arr.iter().map(parse_region_mapping).collect())
            .unwrap_or_default(),
    }
}

/// Parse an `[agent.nudge]` / `[stages.X.nudge]` table into a `NudgeConfig`.
/// Every key is optional; an empty table is inert (each field still inherits
/// the broader level).
pub(super) fn parse_nudge_config(
    where_: &str,
    table: &toml::value::Table,
) -> Result<leviath_runtime::spec::blueprint::NudgeConfig> {
    let mut nudge = leviath_runtime::spec::blueprint::NudgeConfig::default();
    if let Some(enabled) = bool_of(table, "enabled") {
        nudge.enabled = Some(enabled);
    }
    // A negative count is a typo, not "never accept the text", so it is
    // refused rather than inheriting without a word.
    if let Some(max) = count_of(table, where_, "max")? {
        nudge.max = Some(max);
    }
    if let Some(text) = str_of(table, "text") {
        nudge.text = Some(text.trim().to_string());
    }
    Ok(nudge)
}
