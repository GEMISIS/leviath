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

    let name = str_of(agent, "name").unwrap_or("unnamed").to_string();
    let version = str_of(agent, "version").unwrap_or("0.1.0").to_string();
    let description = str_of(agent, "description").unwrap_or("").to_string();

    let max_child_depth = count_of(agent, "[agent]", "max_child_depth")?;

    let entry_stage = str_of(agent, "entry_stage").map(|s| s.to_string());

    let dynamic_tools = bool_of(agent, "dynamic_tools").unwrap_or(false);

    let mut stages = Vec::new();
    if let Some(stages_table) = table_of(&parsed, "stages") {
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

    if let Some(compaction_table) = table_of(&parsed, "compaction") {
        blueprint.compaction_config = Some(parse_compaction_config(compaction_table)?);
    }

    // Parse agent-level security config: [security]
    if let Some(security_table) = table_of(&parsed, "security") {
        blueprint.security = Some(parse_security_config(security_table));
    }

    // Parse agent-level batch_tool_hint override: `[agent] batch_tool_hint`.
    // Absent ⇒ inherit the global config toggle; a per-stage value overrides it.
    if let Some(bth) = bool_of(agent, "batch_tool_hint") {
        blueprint.batch_tool_hint = Some(bth);
    }

    // Parse agent-level shell_hint override: `[agent] shell_hint`. Absent ⇒
    // inherit the global config toggle; a per-stage value overrides it.
    if let Some(sh) = bool_of(agent, "shell_hint") {
        blueprint.shell_hint = Some(sh);
    }

    // Parse agent-level nudge defaults: [agent.nudge]. Absent ⇒ each field
    // inherits the global config's [nudge] section; a per-stage block wins.
    if let Some(nudge_table) = table_of(agent, "nudge") {
        blueprint.nudge = Some(parse_nudge_config("[agent.nudge]", nudge_table)?);
    }

    // Parse the agent's default output shape: [agent.output]. A per-stage
    // block narrows it, and whoever starts the run overrides both.
    if let Some(output_table) = table_of(agent, "output") {
        blueprint.output = Some(parse_output_spec("[agent.output]", output_table)?);
    }

    // Parse agent-level sandbox config: [sandbox]
    if let Some(sandbox_table) = table_of(&parsed, "sandbox") {
        blueprint.sandbox = Some(parse_sandbox_config("", sandbox_table)?);
    }

    // Parse agent-level read-path declarations: [read_paths]. Entries are
    // syntax-checked here so a broken one fails `lev validate`/`lev add`/spawn
    // loudly, instead of degrading the agent at its first out-of-workdir read.
    if let Some(rp_table) = table_of(&parsed, "read_paths") {
        blueprint.read_paths = Some(parse_read_paths(rp_table)?);
    }

    // [safe_commands]: what this agent would like to run unprompted. Inert
    // until the user opts in, so parsing is permissive - a non-string entry is
    // still a hard error, because a list that silently loses members reads as a
    // grant that was made.
    if let Some(sc_table) = table_of(&parsed, "safe_commands") {
        blueprint.safe_commands = Some(parse_safe_commands(sc_table)?);
    }

    // Parse agent-level tool permissions: [tool_permissions]
    if let Some(tp_table) = table_of(&parsed, "tool_permissions") {
        blueprint
            .metadata
            .extend(tool_permission_metadata(tp_table)?);
    }

    // Parse file tracking config: [context.file_tracking]
    if let Some(context_table) = table_of(&parsed, "context")
        && let Some(ft_table) = table_of(context_table, "file_tracking")
    {
        blueprint.file_tracking = Some(parse_file_tracking(ft_table)?);
    }

    // Parse repetition-detection config: [repetition_detection]
    if let Some(rd_table) = table_of(&parsed, "repetition_detection") {
        blueprint.repetition_detection = Some(parse_repetition_detection(rd_table)?);
    }

    // Parse cross-blueprint context transforms: [[transforms]]. Each maps a
    // parent (`from_blueprint`) region onto a child (`to_blueprint`) region when
    // a sub-agent is spawned, optionally transforming the content en route.
    if let Some(transforms_arr) = array_of(&parsed, "transforms") {
        blueprint
            .transforms
            .extend(transforms_arr.iter().map(parse_context_transform));
    }

    Ok(blueprint)
}

mod model;
mod read;
mod regions;
mod sections;
mod stage;

// Glob re-exports, so this split is invisible to every caller and to the
// test module, exactly as `pipeline/mod.rs` does it.
use model::*;
use read::*;
use regions::*;
use sections::*;
use stage::*;

/// Every key `parse_manifest` reads off the `[agent]` table, for the schema
/// guard in `tests.rs`. A list and not a check: the table ignores what it
/// does not know.
#[cfg(test)]
const AGENT_KEYS: &[&str] = &[
    "batch_tool_hint",
    "description",
    "dynamic_tools",
    "entry_stage",
    "max_child_depth",
    "name",
    "nudge",
    "output",
    "shell_hint",
    "version",
];

#[cfg(test)]
mod tests;
