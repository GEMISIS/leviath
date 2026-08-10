//! Manifest parsing for `agent.leviath` files.
//!
//! Pure `TOML` string -> [`Blueprint`] parsing with no filesystem or async
//! dependencies. Filesystem-based manifest discovery (`find_manifest`) lives in
//! `leviath-cli`, since it depends on cli-only path helpers.

use crate::blueprint::{
    ContentTransform, ContextTransform, EdgeTransform, ModelConfig, ModelEntry, RegionMapping,
    StageMode, StuckConfig, TransitionCondition, TransitionEdge,
};
use crate::error::{Error, Result};
use crate::layout::{RegionDefinition, RegionSeed};
use crate::lifecycle::CompactionConfig;
use crate::{Blueprint, ContextLayout, EvictionStrategy, RegionKind, Stage};

/// Parse an agent.leviath TOML manifest into a Blueprint.
pub fn parse_manifest(content: &str) -> Result<Blueprint> {
    let parsed: toml::Value = toml::from_str(content)
        .map_err(|e| Error::Other(format!("Failed to parse agent.leviath: {e}")))?;

    let agent = parsed
        .get("agent")
        .ok_or_else(|| Error::Other("Missing [agent] section".to_string()))?;

    let name = agent
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("unnamed")
        .to_string();
    let version = agent
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.1.0")
        .to_string();
    let description = agent
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let max_child_depth = agent
        .get("max_child_depth")
        .and_then(|v| v.as_integer())
        .map(|v| v as usize);

    let entry_stage = agent
        .get("entry_stage")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Issue #97 escape hatch: `[agent] dynamic_tools` (default false).
    let dynamic_tools = agent
        .get("dynamic_tools")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut stages = Vec::new();
    if let Some(stages_table) = parsed.get("stages").and_then(|v| v.as_table()) {
        for (stage_name, stage_value) in stages_table {
            stages.push(parse_stage(stage_name, stage_value)?);
        }
    }

    if stages.is_empty() {
        stages.push(Stage::new(
            "main".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
        ));
    }

    let (mut regions, mut total_tokens) = match parsed
        .get("context")
        .and_then(|v| v.get("regions"))
        .and_then(|v| v.as_table())
    {
        Some(regions_table) => parse_region_layout(regions_table)?,
        None => (Vec::new(), 0usize),
    };

    if regions.is_empty() {
        // 8000 tokens (~32K chars) for the pinned system region so a substantial
        // stage system_prompt fits in the fallback layout without erroring
        // (see inject_stage_system_prompt); blueprints that need more should
        // declare their own [context.regions].
        regions.push(RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            8000,
        ));
        regions.push(RegionDefinition::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 10,
                eviction_strategy: EvictionStrategy::default(),
            },
            10000,
        ));
        total_tokens = 18000;
    }

    let layout = ContextLayout::new(regions, total_tokens);

    let mut blueprint = Blueprint::new(name, description, stages, layout);
    blueprint.version = version;
    blueprint.max_child_depth = max_child_depth;
    blueprint.entry_stage = entry_stage;
    blueprint.dynamic_tools = dynamic_tools;

    if let Some(compaction_table) = parsed.get("compaction").and_then(|v| v.as_table()) {
        blueprint.compaction_config = Some(parse_compaction_config(compaction_table));
    }

    // Parse agent-level security config: [security]
    if let Some(security_table) = parsed.get("security").and_then(|v| v.as_table()) {
        blueprint.security = Some(parse_security_config(security_table));
    }

    // Parse agent-level batch_tool_hint override: `[agent] batch_tool_hint`.
    // Absent ⇒ inherit the global config toggle; a per-stage value overrides it.
    if let Some(bth) = agent.get("batch_tool_hint").and_then(|v| v.as_bool()) {
        blueprint.batch_tool_hint = Some(bth);
    }

    // Parse agent-level shell_hint override: `[agent] shell_hint`. Absent ⇒
    // inherit the global config toggle; a per-stage value overrides it.
    if let Some(sh) = agent.get("shell_hint").and_then(|v| v.as_bool()) {
        blueprint.shell_hint = Some(sh);
    }

    // Parse agent-level nudge defaults: [agent.nudge]. Absent ⇒ each field
    // inherits the global config's [nudge] section; a per-stage block wins.
    if let Some(nudge_table) = agent.get("nudge").and_then(|v| v.as_table()) {
        blueprint.nudge = Some(parse_nudge_config(nudge_table));
    }

    // Parse the agent's default output shape: [agent.output]. A per-stage
    // block narrows it, and whoever starts the run overrides both.
    if let Some(output_table) = agent.get("output").and_then(|v| v.as_table()) {
        blueprint.output = Some(parse_output_spec(output_table));
    }

    // Parse agent-level sandbox config: [sandbox]
    if let Some(sandbox_table) = parsed.get("sandbox").and_then(|v| v.as_table()) {
        blueprint.sandbox = Some(parse_sandbox_config(sandbox_table)?);
    }

    // Parse agent-level read-path declarations: [read_paths]. Entries are
    // syntax-checked here so a broken one fails `lev validate`/`lev add`/spawn
    // loudly, instead of degrading the agent at its first out-of-workdir read.
    if let Some(rp_table) = parsed.get("read_paths").and_then(|v| v.as_table()) {
        blueprint.read_paths = Some(parse_read_paths(rp_table)?);
    }

    // [safe_commands]: what this agent would like to run unprompted. Inert
    // until the user opts in, so parsing is permissive - a non-string entry is
    // still a hard error, because a list that silently loses members reads as a
    // grant that was made.
    if let Some(sc_table) = parsed.get("safe_commands").and_then(|v| v.as_table()) {
        blueprint.safe_commands = Some(parse_safe_commands(sc_table)?);
    }

    // Parse agent-level tool permissions: [tool_permissions]
    if let Some(tp_table) = parsed.get("tool_permissions").and_then(|v| v.as_table()) {
        blueprint
            .metadata
            .extend(tool_permission_metadata(tp_table)?);
    }

    // Parse file tracking config: [context.file_tracking]
    if let Some(context_table) = parsed.get("context").and_then(|v| v.as_table())
        && let Some(ft_table) = context_table
            .get("file_tracking")
            .and_then(|v| v.as_table())
    {
        blueprint.file_tracking = Some(parse_file_tracking(ft_table));
    }

    // Parse repetition-detection config: [repetition_detection]
    if let Some(rd_table) = parsed
        .get("repetition_detection")
        .and_then(|v| v.as_table())
    {
        blueprint.repetition_detection = Some(parse_repetition_detection(rd_table));
    }

    // Parse cross-blueprint context transforms: [[transforms]]. Each maps a
    // parent (`from_blueprint`) region onto a child (`to_blueprint`) region when
    // a sub-agent is spawned, optionally transforming the content en route.
    if let Some(transforms_arr) = parsed.get("transforms").and_then(|v| v.as_array()) {
        blueprint
            .transforms
            .extend(transforms_arr.iter().map(parse_context_transform));
    }

    Ok(blueprint)
}

mod regions;
mod sections;
mod stage;

// Glob re-exports, so this split is invisible to every caller and to the
// test module, exactly as `pipeline/mod.rs` does it.
use regions::*;
use sections::*;
use stage::*;

#[cfg(test)]
mod tests;
