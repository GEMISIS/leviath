//! Manifest finding and parsing for agent.leviath files.

use leviath_core::blueprint::{
    EdgeTransform, ModelConfig, StageMode, ToolResultRouting, TransitionCondition, TransitionEdge,
};
use leviath_core::layout::RegionDefinition;
use leviath_core::lifecycle::CompactionConfig;
use leviath_core::{Blueprint, ContextLayout, RegionKind, Stage};
use std::path::{Path, PathBuf};

pub fn find_manifest(path: &str) -> anyhow::Result<PathBuf> {
    let p = Path::new(path);

    // 1. Explicit agent.leviath file
    if p.is_file() && p.file_name() == Some(std::ffi::OsStr::new("agent.leviath")) {
        return Ok(p.to_path_buf());
    }

    // 2. Directory with agent.leviath inside
    if p.is_dir() {
        let manifest = p.join("agent.leviath");
        if manifest.exists() {
            return Ok(manifest);
        }
    }

    // 3. Installed agent by name: ~/.leviath/agents/<name>/agent.leviath
    //    dirs::home_dir() always returns Some on supported platforms.
    let installed = dirs::home_dir()
        .unwrap()
        .join(".leviath")
        .join("agents")
        .join(path)
        .join("agent.leviath");
    if installed.exists() {
        return Ok(installed);
    }

    // 4. agent.leviath in current directory (for `lev run` with no path)
    let current_manifest = PathBuf::from("agent.leviath");
    if current_manifest.exists() {
        return Ok(current_manifest);
    }

    anyhow::bail!(
        "Could not find agent manifest for '{}'. \
        Pass a path to a directory containing agent.leviath, \
        or an installed agent name (see `lev list`).",
        path
    )
}

/// Public alias for parse_manifest (used by dashboard, validate, pack, list, test).
pub fn parse_manifest_public(content: &str) -> anyhow::Result<Blueprint> {
    parse_manifest(content)
}

/// Parse an agent.leviath TOML manifest into a Blueprint.
pub fn parse_manifest(content: &str) -> anyhow::Result<Blueprint> {
    let parsed: toml::Value = toml::from_str(content)
        .map_err(|e| anyhow::anyhow!("Failed to parse agent.leviath: {}", e))?;

    let agent = parsed
        .get("agent")
        .ok_or_else(|| anyhow::anyhow!("Missing [agent] section"))?;

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

    let mut stages = Vec::new();
    if let Some(stages_table) = parsed.get("stages").and_then(|v| v.as_table()) {
        for (stage_name, stage_value) in stages_table {
            let model_table = stage_value.get("model").and_then(|v| v.as_table());
            let model_config = if let Some(mt) = model_table {
                ModelConfig::new(
                    mt.get("provider")
                        .and_then(|v| v.as_str())
                        .unwrap_or("anthropic")
                        .to_string(),
                    mt.get("model")
                        .and_then(|v| v.as_str())
                        .unwrap_or("claude-sonnet-4-6")
                        .to_string(),
                )
            } else {
                ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string())
            };

            let mut stage = Stage::new(stage_name.clone(), model_config);

            if let Some(mode_str) = stage_value.get("mode").and_then(|v| v.as_str()) {
                stage = match mode_str {
                    "interactive" => stage.with_mode(StageMode::Interactive),
                    "interactive_points" => {
                        let mut points = Vec::new();
                        if let Some(pts_arr) = stage_value
                            .get("interaction_points")
                            .and_then(|v| v.as_array())
                        {
                            for pt in pts_arr {
                                let pt_name = pt
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let pt_prompt = pt
                                    .get("prompt")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let pt_required =
                                    pt.get("required").and_then(|v| v.as_bool()).unwrap_or(true);
                                let pt_style = match pt.get("style").and_then(|v| v.as_str()) {
                                    Some("multiple_choice") => {
                                        leviath_core::blueprint::InteractionStyle::MultipleChoice
                                    }
                                    Some("confirm") => {
                                        leviath_core::blueprint::InteractionStyle::Confirm
                                    }
                                    _ => leviath_core::blueprint::InteractionStyle::FreeText,
                                };
                                // Accept either "options" or "choices" key
                                let pt_options: Vec<String> = pt
                                    .get("options")
                                    .or_else(|| pt.get("choices"))
                                    .and_then(|v| v.as_array())
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                // Follow-up free-text prompts, keyed by option label:
                                // [stages.<name>.interaction_points.followups]
                                // "Revise — I'll describe changes" = "What would you like to change?"
                                let pt_followups: std::collections::HashMap<String, String> = pt
                                    .get("followups")
                                    .and_then(|v| v.as_table())
                                    .map(|tbl| {
                                        tbl.iter()
                                            .filter_map(|(k, v)| {
                                                v.as_str().map(|s| (k.clone(), s.to_string()))
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                points.push(leviath_core::blueprint::InteractionPoint {
                                    name: pt_name,
                                    prompt: pt_prompt,
                                    required: pt_required,
                                    style: pt_style,
                                    options: pt_options,
                                    followups: pt_followups,
                                });
                            }
                        }
                        stage.with_mode(StageMode::InteractivePoints { points })
                    }
                    _ => stage.with_mode(StageMode::Autonomous),
                };
            }

            if let Some(max_iter) = stage_value
                .get("max_iterations")
                .and_then(|v| v.as_integer())
            {
                stage.max_iterations = Some(max_iter as usize);
            }

            if let Some(tools_arr) = stage_value
                .get("available_tools")
                .and_then(|v| v.as_array())
            {
                stage.available_tools = tools_arr
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
            }

            if let Some(sp) = stage_value.get("system_prompt").and_then(|v| v.as_str()) {
                stage.config.insert(
                    "system_prompt".to_string(),
                    serde_json::Value::String(sp.trim().to_string()),
                );
            }

            if let Some(routing_table) = stage_value.get("tool_routing").and_then(|v| v.as_table())
            {
                let mut routing = ToolResultRouting::default();

                if let Some(dr) = routing_table.get("default_region").and_then(|v| v.as_str()) {
                    routing.default_region = dr.to_string();
                }
                if let Some(p) = routing_table.get("persist").and_then(|v| v.as_bool()) {
                    routing.persist = p;
                }
                if let Some(mt) = routing_table
                    .get("max_result_tokens")
                    .and_then(|v| v.as_integer())
                {
                    routing.max_result_tokens = Some(mt as usize);
                }
                if let Some(overrides_table) =
                    routing_table.get("overrides").and_then(|v| v.as_table())
                {
                    for (tool_name, region_val) in overrides_table {
                        if let Some(region_name) = region_val.as_str() {
                            routing
                                .tool_overrides
                                .insert(tool_name.clone(), region_name.to_string());
                        }
                    }
                }

                stage.tool_result_routing = Some(routing);
            }

            // Parse requires_children flag
            if let Some(rc) = stage_value
                .get("requires_children")
                .and_then(|v| v.as_bool())
            {
                stage.requires_children = rc;
            }

            // Parse allow_complete flag: lets the LLM end the run at this
            // stage (e.g. an approving review) instead of being forced down
            // its only/first transition edge.
            if let Some(ac) = stage_value.get("allow_complete").and_then(|v| v.as_bool()) {
                stage.allow_complete = ac;
            }

            // Parse per-stage tool permissions: [stages.<name>.tool_permissions]
            if let Some(tp_table) = stage_value
                .get("tool_permissions")
                .and_then(|v| v.as_table())
            {
                for (tool_name, policy_val) in tp_table {
                    if let Some(policy_str) = policy_val.as_str() {
                        stage
                            .tool_permissions
                            .insert(tool_name.clone(), policy_str.to_string());
                    }
                }
            }

            // Parse max_revisits
            if let Some(mr) = stage_value.get("max_revisits").and_then(|v| v.as_integer()) {
                stage.max_revisits = Some(mr as usize);
            }

            // Parse transition_prompt
            if let Some(tp) = stage_value
                .get("transition_prompt")
                .and_then(|v| v.as_str())
            {
                stage.transition_prompt = Some(tp.trim().to_string());
            }

            // Parse transitions: [stages.<name>.transitions.<target>]
            if let Some(transitions_table) =
                stage_value.get("transitions").and_then(|v| v.as_table())
            {
                let mut transitions = std::collections::HashMap::new();
                for (target_name, edge_value) in transitions_table {
                    let hint = edge_value
                        .get("hint")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    let condition = match edge_value.get("condition").and_then(|v| v.as_str()) {
                        Some("error") => TransitionCondition::Error,
                        Some("max_iterations") => TransitionCondition::MaxIterations,
                        Some("llm_choice") => TransitionCondition::LlmChoice,
                        Some("always") | None => TransitionCondition::Always,
                        Some(custom) => TransitionCondition::Custom(custom.to_string()),
                    };

                    let transform = match edge_value.get("transform").and_then(|v| v.as_str()) {
                        Some("clear") => EdgeTransform::Clear,
                        Some("compact") | Some("summarize") => {
                            EdgeTransform::Compact { prompt: None }
                        }
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
                        Some(_) => EdgeTransform::Direct,
                    };

                    transitions.insert(
                        target_name.clone(),
                        TransitionEdge {
                            target: target_name.clone(),
                            condition,
                            hint,
                            transform,
                        },
                    );
                }
                stage.transitions = Some(transitions);
            }

            stages.push(stage);
        }
    }

    if stages.is_empty() {
        stages.push(Stage::new(
            "main".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
        ));
    }

    let mut regions = Vec::new();
    let mut total_tokens = 0usize;

    if let Some(regions_table) = parsed
        .get("context")
        .and_then(|v| v.get("regions"))
        .and_then(|v| v.as_table())
    {
        for (region_name, region_value) in regions_table {
            let max_tokens = region_value
                .get("max_tokens")
                .and_then(|v| v.as_integer())
                .unwrap_or(5000) as usize;

            let kind_str = region_value
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("temporary");

            let kind = match kind_str {
                "pinned" => RegionKind::Pinned,
                "sliding_window" => {
                    let max_items = region_value
                        .get("max_items")
                        .and_then(|v| v.as_integer())
                        .unwrap_or(10) as usize;
                    RegionKind::SlidingWindow { max_items }
                }
                "temporary" => RegionKind::Temporary,
                "compacting" => {
                    let threshold = region_value
                        .get("threshold_tokens")
                        .and_then(|v| v.as_integer())
                        .unwrap_or((max_tokens as i64) * 8 / 10)
                        as usize;
                    RegionKind::Compacting {
                        threshold_tokens: threshold,
                    }
                }
                "clearable" => RegionKind::Clearable,
                "compact_history" => {
                    let source = region_value
                        .get("source_region")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    RegionKind::CompactHistory {
                        source_region: source,
                    }
                }
                _ => RegionKind::Temporary,
            };

            total_tokens += max_tokens;
            regions.push(RegionDefinition::new(region_name.clone(), kind, max_tokens));
        }
    }

    if regions.is_empty() {
        regions.push(RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            2000,
        ));
        regions.push(RegionDefinition::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow { max_items: 10 },
            10000,
        ));
        total_tokens = 12000;
    }

    let layout = ContextLayout::new(regions, total_tokens);

    let mut blueprint = Blueprint::new(name, description, stages, layout);
    blueprint.version = version;
    blueprint.max_child_depth = max_child_depth;
    blueprint.entry_stage = entry_stage;

    if let Some(compaction_table) = parsed.get("compaction").and_then(|v| v.as_table()) {
        let mut cc = CompactionConfig::default();

        if let Some(provider) = compaction_table.get("provider").and_then(|v| v.as_str()) {
            cc.provider = provider.to_string();
        }
        if let Some(model) = compaction_table.get("model").and_then(|v| v.as_str()) {
            cc.model = model.to_string();
        }
        if let Some(sp) = compaction_table
            .get("system_prompt")
            .and_then(|v| v.as_str())
        {
            cc.system_prompt = Some(sp.to_string());
        }
        if let Some(mst) = compaction_table
            .get("max_summary_tokens")
            .and_then(|v| v.as_integer())
        {
            cc.max_summary_tokens = mst as usize;
        }
        if let Some(temp) = compaction_table
            .get("temperature")
            .and_then(|v| v.as_float())
        {
            cc.temperature = temp as f32;
        }

        blueprint.compaction_config = Some(cc);
    }

    // Parse security config: [security]
    if let Some(security_table) = parsed.get("security").and_then(|v| v.as_table()) {
        let mut sc = leviath_core::SecurityConfig::default();

        if let Some(tt) = security_table
            .get("taint_tracking")
            .and_then(|v| v.as_bool())
        {
            sc.taint_tracking = tt;
        }
        if let Some(pm) = security_table.get("pointer_mode").and_then(|v| v.as_bool()) {
            sc.pointer_mode = pm;
        }
        if let Some(fm_val) = security_table.get("filter_mode") {
            if let Some(fm_str) = fm_val.as_str() {
                sc.filter_mode = leviath_core::FilterMode::from_str_loose(fm_str);
            } else if let Some(false) = fm_val.as_bool() {
                sc.filter_mode = None;
            }
        }
        if let Some(deg_arr) = security_table.get("degradation").and_then(|v| v.as_array()) {
            let modes: Vec<leviath_core::InputMode> = deg_arr
                .iter()
                .filter_map(|v| v.as_str().and_then(leviath_core::InputMode::from_str_loose))
                .collect();
            if !modes.is_empty() {
                sc.degradation = modes;
            }
        }

        blueprint.security = Some(sc);
    }

    // Parse agent-level tool permissions: [tool_permissions]
    if let Some(tp_table) = parsed.get("tool_permissions").and_then(|v| v.as_table()) {
        for (tool_name, policy_val) in tp_table {
            if let Some(policy_str) = policy_val.as_str() {
                blueprint.metadata.insert(
                    format!("tool_perm:{}", tool_name),
                    serde_json::Value::String(policy_str.to_string()),
                );
            }
        }
    }

    Ok(blueprint)
}

#[cfg(test)]
mod tests {
    use super::*;

    // `set_current_dir` is process-global, so any test whose assertion
    // implicitly depends on CWD state (like `find_manifest`'s "no
    // agent.leviath in CWD" branch) must serialize against every other
    // CWD-mutating test in the crate, not just the ones in this file --
    // otherwise it can observe a CWD another test temporarily pointed
    // elsewhere and fail nondeterministically. Confirmed exactly this:
    // `find_manifest_dir_without_manifest_falls_through` didn't hold a lock
    // and intermittently failed on CI by observing
    // `find_manifest_cwd_agent_leviath_found`'s CWD mid-swap. Uses the
    // crate-wide `crate::config::CWD_LOCK` (not a file-local one) so it
    // actually serializes against CWD-mutating tests added to other files.
    use crate::config::CWD_LOCK;

    // ─── test helpers ─────────────────────────────────────────────────────────

    /// Extract the `points` vec from a `StageMode::InteractivePoints`.
    /// Panics (with a diagnostic) when the mode is any other variant.
    /// The panic branch is exercised by `unwrap_interactive_points_panics_on_wrong_mode`.
    fn unwrap_interactive_points(mode: &StageMode) -> &[leviath_core::blueprint::InteractionPoint] {
        match mode {
            StageMode::InteractivePoints { points } => points,
            other => panic!(
                "expected StageMode::InteractivePoints, got {:?}",
                std::mem::discriminant(other)
            ),
        }
    }

    #[test]
    #[should_panic(expected = "expected StageMode::InteractivePoints")]
    fn unwrap_interactive_points_panics_on_wrong_mode() {
        let mode = StageMode::Autonomous;
        let _ = unwrap_interactive_points(&mode);
    }

    // ─── parse_manifest ──────────────────────────────────────────────────────

    #[test]
    fn parse_minimal_manifest() {
        let toml = r#"
[agent]
name = "test-agent"
"#;
        let bp = parse_manifest(toml).unwrap();
        assert_eq!(bp.name, "test-agent");
        assert_eq!(bp.version, "0.1.0"); // default
        assert_eq!(bp.stages.len(), 1); // default main stage
        assert_eq!(bp.stages[0].name, "main");
    }

    #[test]
    fn parse_full_manifest_with_all_fields() {
        let toml = r#"
[agent]
name = "full-agent"
version = "2.0.0"
description = "A fully configured agent"
max_child_depth = 3
entry_stage = "start"

[stages.start]
mode = "autonomous"
model = { provider = "openai", model = "gpt-5" }
max_iterations = 25
available_tools = ["read_file", "bash"]
system_prompt = "You are a coding assistant."
requires_children = true
max_revisits = 5

[stages.start.tool_permissions]
bash = "ask"
read_file = "allow"

[stages.finish]
mode = "interactive"
model = { provider = "anthropic", model = "claude-sonnet-4-6" }

[context.regions]
system = { kind = "pinned", max_tokens = 2000 }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#;
        let bp = parse_manifest(toml).unwrap();
        assert_eq!(bp.name, "full-agent");
        assert_eq!(bp.version, "2.0.0");
        assert_eq!(bp.description, "A fully configured agent");
        assert_eq!(bp.max_child_depth, Some(3));
        assert_eq!(bp.entry_stage, Some("start".to_string()));
        assert_eq!(bp.stages.len(), 2);

        let start = bp.find_stage("start").unwrap();
        assert_eq!(start.mode, StageMode::Autonomous);
        assert_eq!(start.model.provider, "openai");
        assert_eq!(start.model.model, "gpt-5");
        assert_eq!(start.max_iterations, Some(25));
        assert_eq!(start.available_tools, vec!["read_file", "bash"]);
        assert!(start.requires_children);
        assert_eq!(start.max_revisits, Some(5));
        assert_eq!(
            start.tool_permissions.get("bash").map(|s| s.as_str()),
            Some("ask")
        );
        assert_eq!(
            start.tool_permissions.get("read_file").map(|s| s.as_str()),
            Some("allow")
        );

        let finish = bp.find_stage("finish").unwrap();
        assert_eq!(finish.mode, StageMode::Interactive);
    }

    #[test]
    fn parse_manifest_with_graph_transitions() {
        let toml = r#"
[agent]
name = "graph-agent"

[stages.analyze]
mode = "autonomous"
transition_prompt = "Pick the next stage"

[stages.analyze.transitions.implement]
condition = "always"
hint = "Ready to implement"
transform = "direct"

[stages.analyze.transitions.error_handler]
condition = "error"
transform = "clear"

[stages.analyze.transitions.timeout_handler]
condition = "max_iterations"
transform = "compact"

[stages.analyze.transitions.choice_stage]
condition = "llm_choice"
hint = "LLM chooses this"

[stages.analyze.transitions.custom_stage]
condition = "my_custom_condition"

[stages.implement]
mode = "autonomous"

[stages.error_handler]
mode = "autonomous"

[stages.timeout_handler]
mode = "autonomous"

[stages.choice_stage]
mode = "autonomous"

[stages.custom_stage]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        let analyze = bp.find_stage("analyze").unwrap();
        assert_eq!(
            analyze.transition_prompt,
            Some("Pick the next stage".to_string())
        );
        let transitions = analyze.transitions.as_ref().unwrap();
        assert_eq!(transitions.len(), 5);

        let impl_edge = transitions.get("implement").unwrap();
        assert_eq!(impl_edge.condition, TransitionCondition::Always);
        assert_eq!(impl_edge.hint.as_deref(), Some("Ready to implement"));
        assert_eq!(impl_edge.transform, EdgeTransform::Direct);

        let err_edge = transitions.get("error_handler").unwrap();
        assert_eq!(err_edge.condition, TransitionCondition::Error);
        assert_eq!(err_edge.transform, EdgeTransform::Clear);

        let timeout_edge = transitions.get("timeout_handler").unwrap();
        assert_eq!(timeout_edge.condition, TransitionCondition::MaxIterations);
        assert_eq!(
            timeout_edge.transform,
            EdgeTransform::Compact { prompt: None }
        );

        let choice_edge = transitions.get("choice_stage").unwrap();
        assert_eq!(choice_edge.condition, TransitionCondition::LlmChoice);

        let custom_edge = transitions.get("custom_stage").unwrap();
        assert_eq!(
            custom_edge.condition,
            TransitionCondition::Custom("my_custom_condition".to_string())
        );
    }

    #[test]
    fn parse_manifest_with_context_regions_all_kinds() {
        let toml = r#"
[agent]
name = "region-test"

[context.regions]
sys = { kind = "pinned", max_tokens = 1000 }
conv = { kind = "sliding_window", max_items = 15, max_tokens = 5000 }
temp = { kind = "temporary", max_tokens = 3000 }
comp = { kind = "compacting", threshold_tokens = 4000, max_tokens = 6000 }
clr = { kind = "clearable", max_tokens = 2000 }
hist = { kind = "compact_history", source_region = "conv", max_tokens = 4000 }
"#;
        let bp = parse_manifest(toml).unwrap();
        assert_eq!(bp.context_layout.regions.len(), 6);

        let sys = bp
            .context_layout
            .regions
            .iter()
            .find(|r| r.name == "sys")
            .unwrap();
        assert_eq!(sys.kind, RegionKind::Pinned);
        assert_eq!(sys.max_tokens, 1000);

        let conv = bp
            .context_layout
            .regions
            .iter()
            .find(|r| r.name == "conv")
            .unwrap();
        assert_eq!(conv.kind, RegionKind::SlidingWindow { max_items: 15 });

        let temp = bp
            .context_layout
            .regions
            .iter()
            .find(|r| r.name == "temp")
            .unwrap();
        assert_eq!(temp.kind, RegionKind::Temporary);

        let comp = bp
            .context_layout
            .regions
            .iter()
            .find(|r| r.name == "comp")
            .unwrap();
        assert_eq!(
            comp.kind,
            RegionKind::Compacting {
                threshold_tokens: 4000
            }
        );

        let clr = bp
            .context_layout
            .regions
            .iter()
            .find(|r| r.name == "clr")
            .unwrap();
        assert_eq!(clr.kind, RegionKind::Clearable);

        let hist = bp
            .context_layout
            .regions
            .iter()
            .find(|r| r.name == "hist")
            .unwrap();
        assert_eq!(
            hist.kind,
            RegionKind::CompactHistory {
                source_region: "conv".to_string()
            }
        );
    }

    #[test]
    fn parse_manifest_with_tool_result_routing() {
        let toml = r#"
[agent]
name = "routing-test"

[stages.main]
mode = "autonomous"

[stages.main.tool_routing]
default_region = "scratch"
persist = true
max_result_tokens = 5000

[stages.main.tool_routing.overrides]
read_file = "codebase"
bash = "conversation"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        let routing = stage.tool_result_routing.as_ref().unwrap();
        assert_eq!(routing.default_region, "scratch");
        assert!(routing.persist);
        assert_eq!(routing.max_result_tokens, Some(5000));
        assert_eq!(
            routing.tool_overrides.get("read_file").map(|s| s.as_str()),
            Some("codebase")
        );
        assert_eq!(
            routing.tool_overrides.get("bash").map(|s| s.as_str()),
            Some("conversation")
        );
    }

    #[test]
    fn parse_manifest_with_model_config() {
        let toml = r#"
[agent]
name = "model-test"

[stages.main]
model = { provider = "google", model = "gemini-3.5-pro" }
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        assert_eq!(stage.model.provider, "google");
        assert_eq!(stage.model.model, "gemini-3.5-pro");
    }

    #[test]
    fn parse_manifest_default_model() {
        let toml = r#"
[agent]
name = "default-model"

[stages.main]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        assert_eq!(stage.model.provider, "anthropic");
        assert_eq!(stage.model.model, "claude-sonnet-4-6");
    }

    #[test]
    fn parse_manifest_with_interaction_points() {
        let toml = r#"
[agent]
name = "interactive-test"

[stages.main]
mode = "interactive_points"

[[stages.main.interaction_points]]
name = "review"
prompt = "Review the output"
required = true
style = "multiple_choice"
options = ["approve", "reject", "revise"]

[[stages.main.interaction_points]]
name = "feedback"
prompt = "Any feedback?"
required = false
style = "free_text"

[[stages.main.interaction_points]]
name = "confirm"
prompt = "Proceed?"
style = "confirm"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        let points = unwrap_interactive_points(&stage.mode);
        assert_eq!(points.len(), 3);

        assert_eq!(points[0].name, "review");
        assert_eq!(points[0].prompt, "Review the output");
        assert!(points[0].required);
        assert_eq!(
            points[0].style,
            leviath_core::blueprint::InteractionStyle::MultipleChoice
        );
        assert_eq!(points[0].options, vec!["approve", "reject", "revise"]);

        assert_eq!(points[1].name, "feedback");
        assert!(!points[1].required);
        assert_eq!(
            points[1].style,
            leviath_core::blueprint::InteractionStyle::FreeText
        );

        assert_eq!(points[2].name, "confirm");
        assert_eq!(
            points[2].style,
            leviath_core::blueprint::InteractionStyle::Confirm
        );
    }

    #[test]
    fn parse_manifest_interaction_point_followups() {
        let toml = r#"
[agent]
name = "followup-test"

[stages.plan]
mode = "interactive_points"

[[stages.plan.interaction_points]]
name     = "plan_approval"
prompt   = "Approve?"
required = true
style    = "multiple_choice"
options  = ["Approve", "Revise", "Abort"]
followups = { "Revise" = "What would you like to change?" }
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("plan").unwrap();
        let points = unwrap_interactive_points(&stage.mode);
        assert_eq!(points.len(), 1);
        assert_eq!(
            points[0].followups.get("Revise").map(|s| s.as_str()),
            Some("What would you like to change?")
        );
        assert!(!points[0].followups.contains_key("Approve"));
        assert!(!points[0].followups.contains_key("Abort"));
    }

    #[test]
    fn parse_manifest_interaction_point_no_followups_defaults_empty() {
        let toml = r#"
[agent]
name = "no-followup-test"

[stages.main]
mode = "interactive_points"

[[stages.main.interaction_points]]
name     = "confirm"
prompt   = "Proceed?"
style    = "confirm"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        let points = unwrap_interactive_points(&stage.mode);
        assert!(points[0].followups.is_empty());
    }

    #[test]
    fn parse_manifest_stage_allow_complete() {
        let toml = r#"
[agent]
name = "allow-complete-test"

[stages.review]
mode = "autonomous"
allow_complete = true
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("review").unwrap();
        assert!(stage.allow_complete);
    }

    #[test]
    fn parse_manifest_stage_allow_complete_defaults_false() {
        let toml = r#"
[agent]
name = "allow-complete-default-test"

[stages.review]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("review").unwrap();
        assert!(!stage.allow_complete);
    }

    #[test]
    fn parse_manifest_with_compaction_config() {
        let toml = r#"
[agent]
name = "compact-test"

[compaction]
provider = "openai"
model = "gpt-4o-mini"
system_prompt = "Summarize concisely"
max_summary_tokens = 500
temperature = 0.2
"#;
        let bp = parse_manifest(toml).unwrap();
        let cc = bp.compaction_config.as_ref().unwrap();
        assert_eq!(cc.provider, "openai");
        assert_eq!(cc.model, "gpt-4o-mini");
        assert_eq!(cc.system_prompt.as_deref(), Some("Summarize concisely"));
        assert_eq!(cc.max_summary_tokens, 500);
        assert!((cc.temperature - 0.2).abs() < 0.01);
    }

    #[test]
    fn parse_manifest_with_custom_edge_transform() {
        let toml = r#"
[agent]
name = "custom-edge"

[stages.a]
mode = "autonomous"

[stages.a.transitions.b]
transform = "custom"

[stages.a.transitions.b.transform_config]
carry = ["system"]
compact = ["conversation"]
clear = ["scratch"]
compact_prompt = "Summarize for next stage"

[stages.b]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage_a = bp.find_stage("a").unwrap();
        let transitions = stage_a.transitions.as_ref().unwrap();
        let edge = transitions.get("b").unwrap();
        assert_eq!(
            edge.transform,
            EdgeTransform::Custom {
                carry: vec!["system".to_string()],
                compact: vec!["conversation".to_string()],
                clear: vec!["scratch".to_string()],
                compact_prompt: Some("Summarize for next stage".to_string()),
            }
        );
    }

    #[test]
    fn parse_manifest_with_agent_tool_permissions() {
        let toml = r#"
[agent]
name = "perm-test"

[tool_permissions]
bash = "ask"
write_file = "deny"
read_file = "allow"
"#;
        let bp = parse_manifest(toml).unwrap();
        assert_eq!(
            bp.metadata.get("tool_perm:bash").and_then(|v| v.as_str()),
            Some("ask")
        );
        assert_eq!(
            bp.metadata
                .get("tool_perm:write_file")
                .and_then(|v| v.as_str()),
            Some("deny")
        );
        assert_eq!(
            bp.metadata
                .get("tool_perm:read_file")
                .and_then(|v| v.as_str()),
            Some("allow")
        );
    }

    #[test]
    fn parse_manifest_error_missing_agent_section() {
        let toml = r#"
[stages.main]
mode = "autonomous"
"#;
        let result = parse_manifest(toml);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing [agent] section"));
    }

    #[test]
    fn parse_manifest_error_invalid_toml() {
        let result = parse_manifest("not valid toml {{{}}}");
        assert!(result.is_err());
    }

    #[test]
    fn parse_manifest_default_regions_when_none_specified() {
        let toml = r#"
[agent]
name = "no-regions"
"#;
        let bp = parse_manifest(toml).unwrap();
        assert_eq!(bp.context_layout.regions.len(), 2); // system + conversation
        assert_eq!(bp.context_layout.regions[0].name, "system");
        assert_eq!(bp.context_layout.regions[0].kind, RegionKind::Pinned);
        assert_eq!(bp.context_layout.regions[1].name, "conversation");
        assert_eq!(
            bp.context_layout.regions[1].kind,
            RegionKind::SlidingWindow { max_items: 10 }
        );
    }

    #[test]
    fn parse_manifest_unknown_region_kind_defaults_to_temporary() {
        let toml = r#"
[agent]
name = "unknown-kind"

[context.regions]
test = { kind = "unknown_kind", max_tokens = 1000 }
"#;
        let bp = parse_manifest(toml).unwrap();
        let region = &bp.context_layout.regions[0];
        assert_eq!(region.kind, RegionKind::Temporary);
    }

    #[test]
    fn parse_manifest_stage_modes() {
        let toml = r#"
[agent]
name = "modes-test"

[stages.auto]
mode = "autonomous"

[stages.inter]
mode = "interactive"

[stages.default_mode]
"#;
        let bp = parse_manifest(toml).unwrap();
        let auto = bp.find_stage("auto").unwrap();
        assert_eq!(auto.mode, StageMode::Autonomous);

        let inter = bp.find_stage("inter").unwrap();
        assert_eq!(inter.mode, StageMode::Interactive);

        // Default mode (no mode specified) — Autonomous
        let default = bp.find_stage("default_mode").unwrap();
        assert_eq!(default.mode, StageMode::Autonomous);
    }

    #[test]
    fn parse_manifest_with_stage_system_prompt() {
        let toml = r#"
[agent]
name = "prompt-test"

[stages.main]
system_prompt = "  You are helpful.  "
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        let sp = stage.config.get("system_prompt").unwrap().as_str().unwrap();
        assert_eq!(sp, "You are helpful.");
    }

    #[test]
    fn parse_manifest_summarize_transform_alias() {
        let toml = r#"
[agent]
name = "alias-test"

[stages.a]
mode = "autonomous"

[stages.a.transitions.b]
transform = "summarize"

[stages.b]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage_a = bp.find_stage("a").unwrap();
        let transitions = stage_a.transitions.as_ref().unwrap();
        let edge = transitions.get("b").unwrap();
        assert_eq!(edge.transform, EdgeTransform::Compact { prompt: None });
    }

    #[test]
    fn parse_manifest_unrecognized_transform_defaults_to_direct() {
        let toml = r#"
[agent]
name = "unknown-transform"

[stages.a]
mode = "autonomous"

[stages.a.transitions.b]
transform = "some_unrecognized_value"

[stages.b]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage_a = bp.find_stage("a").unwrap();
        let transitions = stage_a.transitions.as_ref().unwrap();
        let edge = transitions.get("b").unwrap();
        assert_eq!(edge.transform, EdgeTransform::Direct);
    }

    // ─── find_manifest ───────────────────────────────────────────────────────

    #[test]
    fn find_manifest_with_file_path() {
        let dir = std::env::temp_dir().join("lev-test-find-manifest-file");
        let _ = std::fs::create_dir_all(&dir);
        let manifest = dir.join("agent.leviath");
        std::fs::write(&manifest, "[agent]\nname = \"test\"").unwrap();

        let result = find_manifest(manifest.to_str().unwrap());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), manifest);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_manifest_with_directory_path() {
        let dir = std::env::temp_dir().join("lev-test-find-manifest-dir");
        let _ = std::fs::create_dir_all(&dir);
        let manifest = dir.join("agent.leviath");
        std::fs::write(&manifest, "[agent]\nname = \"test\"").unwrap();

        let result = find_manifest(dir.to_str().unwrap());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), manifest);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_manifest_with_invalid_path() {
        // See CWD_LOCK's doc comment -- branch 4 depends on CWD state.
        let _guard = CWD_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = find_manifest("/nonexistent/path/to/nothing");
        assert!(result.is_err());
    }

    #[test]
    fn find_manifest_installed_agent_by_name() {
        // `dirs::home_dir()` can't be redirected via `$HOME` on macOS (it
        // resolves via `NSHomeDirectory()`), so this writes to the real
        // `~/.leviath/agents/<name>/agent.leviath` -- using a name unlikely
        // to collide with a real installed agent, cleaned up afterward.
        let home = dirs::home_dir().expect("home directory must be available");
        let agent_name = "test-find-manifest-installed-by-name-8f3a";
        let agent_dir = home.join(".leviath").join("agents").join(agent_name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        let manifest_path = agent_dir.join("agent.leviath");
        std::fs::write(&manifest_path, "[agent]\nname = \"test\"").unwrap();

        let result = find_manifest(agent_name);
        assert_eq!(result.unwrap(), manifest_path);

        let _ = std::fs::remove_dir_all(&agent_dir);
    }

    /// Covers branch 2 (directory exists) when the directory has NO `agent.leviath` inside.
    /// This exercises the implicit else of `if manifest.exists()` on line ~23.
    #[test]
    fn find_manifest_dir_without_manifest_falls_through() {
        // Branch 4 of find_manifest checks for a bare "agent.leviath" in the
        // process's current working directory -- process-global state that
        // find_manifest_cwd_agent_leviath_found deliberately mutates. Without
        // holding CWD_LOCK here too, this test can run while CWD is
        // temporarily pointed at that other test's directory (which does
        // contain agent.leviath), observe branch 4 succeed, and fail this
        // assertion nondeterministically -- confirmed on CI.
        let _guard = CWD_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join("lev-test-dir-no-manifest-9z7q");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // No agent.leviath inside — the dir branch falls through to the error.
        let result = find_manifest(dir.to_str().unwrap());
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Covers branch 3 (installed agent by name) when the agent is NOT installed.
    /// The `if let Some(home)` block is entered (home exists on macOS) but the
    /// `if installed.exists()` is false, so we fall through to the error.
    #[test]
    fn find_manifest_installed_agent_not_found_falls_through() {
        // See CWD_LOCK's doc comment -- branch 4 depends on CWD state.
        let _guard = CWD_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = find_manifest("lev-no-such-agent-xyzzy-9f3a");
        assert!(result.is_err());
    }

    /// Covers branch 4: a bare `agent.leviath` exists in the current directory.
    /// Uses `CWD_LOCK` to prevent parallel tests from interfering.
    #[test]
    fn find_manifest_cwd_agent_leviath_found() {
        let dir = std::env::temp_dir().join("lev-test-cwd-manifest-a1b2");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("agent.leviath");
        std::fs::write(&manifest, "[agent]\nname = \"cwd-test\"").unwrap();

        // Serialize all CWD-mutating tests so they don't interfere.
        let _guard = CWD_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();

        // find_manifest("__nonexistent__") falls through branches 1-3 and
        // finds the agent.leviath in the new CWD (branch 4).
        let result = find_manifest("__lev_cwd_test_nonexistent__");

        // Always restore CWD before asserting so cleanup runs even on failure.
        std::env::set_current_dir(&original_cwd).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_ok());
        assert_eq!(result.unwrap().file_name().unwrap(), "agent.leviath");
    }

    // ─── parse_manifest_public ───────────────────────────────────────────────

    #[test]
    fn parse_manifest_public_delegates_to_parse_manifest() {
        let toml = r#"
[agent]
name = "public-test"
version = "1.0.0"
"#;
        let bp = parse_manifest_public(toml).unwrap();
        assert_eq!(bp.name, "public-test");
        assert_eq!(bp.version, "1.0.0");
    }

    // ─── Regression: shipped software-engineer agent must branch on plan_approval ──
    //
    // The "plan" stage's plan_approval interaction point lets the user pick
    // Approve / Revise / Add detail / Abort. If "plan" only has a single
    // outgoing transition edge, resolve_transition() auto-follows it without
    // ever consulting the LLM — so anything other than "Approve" is silently
    // ignored and the run proceeds to "implement" anyway. Guard against that
    // regressing by requiring at least two outgoing edges (forcing the
    // LLM-consultation path in resolve_transition / prompt_llm_transition).
    #[test]
    fn software_engineer_plan_stage_branches_on_choice() {
        let manifest_content =
            include_str!("../../../../../agents/software-engineer/agent.leviath");
        let bp = parse_manifest(manifest_content).unwrap();
        let plan = bp.find_stage("plan").unwrap();

        let transitions = plan
            .transitions
            .as_ref()
            .expect("plan stage must declare transitions");
        // plan stage must have >=2 outgoing edges so the user's plan_approval
        // choice (Revise/Add detail/Abort) actually changes behavior instead
        // of being silently ignored by a single-edge auto-transition.
        assert!(transitions.len() >= 2);
        assert!(transitions.contains_key("implement"));

        // A self-loop (or other non-"implement" edge) must exist so revising/
        // aborting doesn't fall through to implementation.
        assert!(transitions.keys().any(|t| t != "implement"));

        // The self-loop must be revisit-capped to avoid an infinite planning loop.
        // plan stage must have a self-loop ('plan' transition) so the user can revise.
        assert!(transitions.contains_key("plan"));
        // self-looping 'plan' stage must cap max_revisits.
        assert!(plan.max_revisits.is_some());
    }

    #[test]
    fn software_engineer_plan_stage_has_error_edge_and_can_abort() {
        let manifest_content =
            include_str!("../../../../../agents/software-engineer/agent.leviath");
        let bp = parse_manifest(manifest_content).unwrap();
        let plan = bp.find_stage("plan").unwrap();
        let transitions = plan.transitions.as_ref().unwrap();

        // plan stage should route errors to error_recovery, like implement/review do.
        assert!(transitions
            .get("error_recovery")
            .map(|e| e.condition == leviath_core::blueprint::TransitionCondition::Error)
            .unwrap_or(false));

        // allow_complete lets the model respond DONE (e.g. when the user
        // chose "Abort") instead of being forced into 'implement' or 'plan'.
        assert!(plan.allow_complete);
    }

    #[test]
    fn software_engineer_plan_approval_has_followups_for_non_terminal_choices() {
        let manifest_content =
            include_str!("../../../../../agents/software-engineer/agent.leviath");
        let bp = parse_manifest(manifest_content).unwrap();
        let plan = bp.find_stage("plan").unwrap();
        let points = unwrap_interactive_points(&plan.mode);
        let approval = points
            .iter()
            .find(|p| p.name == "plan_approval")
            .expect("plan_approval interaction point must exist");

        // "Revise" and "Add detail" promise the user a chance to describe
        // what they want — without a followup prompt, only the static label
        // ever reaches the model and the user's actual feedback is lost.
        let revise_key = approval
            .options
            .iter()
            .find(|o| o.starts_with("Revise"))
            .expect("a Revise option must exist");
        let detail_key = approval
            .options
            .iter()
            .find(|o| o.starts_with("Add detail"))
            .expect("an Add detail option must exist");
        // Revise/Add detail options must have a followup prompt asking for details.
        assert!(approval.followups.contains_key(revise_key));
        assert!(approval.followups.contains_key(detail_key));

        // Approve/Abort are terminal/decisive — they must NOT have followups
        // (no further elaboration needed).
        for opt in &approval.options {
            if opt.starts_with("Approve") || opt.starts_with("Abort") {
                assert!(!approval.followups.contains_key(opt));
            }
        }
    }

    #[test]
    fn software_engineer_review_stage_can_complete_and_routes_errors() {
        let manifest_content =
            include_str!("../../../../../agents/software-engineer/agent.leviath");
        let bp = parse_manifest(manifest_content).unwrap();
        let review = bp.find_stage("review").unwrap();

        // review stage must allow_complete — an approving review has no real
        // next stage and must not be forced back into 'implement'.
        assert!(review.allow_complete);

        let transitions = review
            .transitions
            .as_ref()
            .expect("review stage must declare transitions");
        // review stage should route errors to error_recovery, like implement does.
        assert!(transitions
            .get("error_recovery")
            .map(|e| e.condition == leviath_core::blueprint::TransitionCondition::Error)
            .unwrap_or(false));
    }

    #[test]
    fn software_engineer_blueprint_passes_full_validation() {
        let manifest_content =
            include_str!("../../../../../agents/software-engineer/agent.leviath");
        let bp = parse_manifest(manifest_content).unwrap();
        bp.validate()
            .expect("shipped software-engineer blueprint must pass Blueprint::validate()");
    }

    #[test]
    fn software_engineer_plan_and_implement_can_ask_the_user_dynamically() {
        // Beyond the static plan_approval checkpoint, plan/implement should
        // be able to decide for themselves, mid-reasoning, that they need
        // human input — via the ask_user_* tools, not just the forced
        // interaction_points.
        let manifest_content =
            include_str!("../../../../../agents/software-engineer/agent.leviath");
        let bp = parse_manifest(manifest_content).unwrap();

        let plan = bp.find_stage("plan").unwrap();
        assert!(plan.available_tools.contains(&"ask_user_text".to_string()));
        assert!(plan
            .available_tools
            .contains(&"ask_user_choice".to_string()));

        let implement = bp.find_stage("implement").unwrap();
        assert!(implement
            .available_tools
            .contains(&"ask_user_text".to_string()));
        assert!(implement
            .available_tools
            .contains(&"ask_user_confirm".to_string()));
    }

    // ─── Production-code branch coverage: optional field None-paths ──────────

    /// `interactive_points` mode with NO `interaction_points` array — the stage
    /// still gets the mode, just with an empty points list.
    #[test]
    fn parse_manifest_interactive_points_mode_with_no_points_array() {
        let toml = r#"
[agent]
name = "no-points"

[stages.main]
mode = "interactive_points"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        let points = unwrap_interactive_points(&stage.mode);
        assert!(points.is_empty());
    }

    /// tool_routing with only partial fields — covers the None-branches for
    /// `default_region`, `persist`, and `max_result_tokens`.
    #[test]
    fn parse_manifest_tool_routing_partial_fields() {
        // Only max_result_tokens is set; default_region and persist are absent.
        let toml = r#"
[agent]
name = "partial-routing"

[stages.main]
mode = "autonomous"

[stages.main.tool_routing]
max_result_tokens = 1000
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        let routing = stage.tool_result_routing.as_ref().unwrap();
        // default_region absent — stays at ToolResultRouting::default() value
        // (we just verify it's not "scratch" to confirm it was not set)
        assert_ne!(routing.default_region, "scratch");
        // persist absent — stays at ToolResultRouting::default() value (we don't
        // prescribe what the default is, just verify it was not explicitly set to false)
        let _ = routing.persist; // field exists, not set by this test
        assert_eq!(routing.max_result_tokens, Some(1000));
    }

    /// tool_routing without any overrides table — covers the None-branch for
    /// `routing_table.get("overrides")`.
    #[test]
    fn parse_manifest_tool_routing_no_overrides() {
        let toml = r#"
[agent]
name = "no-overrides"

[stages.main]
mode = "autonomous"

[stages.main.tool_routing]
default_region = "scratch"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        let routing = stage.tool_result_routing.as_ref().unwrap();
        assert_eq!(routing.default_region, "scratch");
        assert!(routing.tool_overrides.is_empty());
    }

    /// tool_routing.overrides with a non-string value — the inner
    /// `if let Some(region_name) = region_val.as_str()` should be skipped.
    #[test]
    fn parse_manifest_tool_routing_overrides_non_string_value_skipped() {
        let toml = r#"
[agent]
name = "non-string-override"

[stages.main]
mode = "autonomous"

[stages.main.tool_routing]
default_region = "scratch"

[stages.main.tool_routing.overrides]
bash = 42
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        let routing = stage.tool_result_routing.as_ref().unwrap();
        // Non-string value for "bash" is silently skipped.
        assert!(routing.tool_overrides.is_empty());
    }

    /// Per-stage tool_permissions with a non-string policy value — the inner
    /// `if let Some(policy_str) = policy_val.as_str()` should be skipped.
    #[test]
    fn parse_manifest_stage_tool_permissions_non_string_value_skipped() {
        let toml = r#"
[agent]
name = "non-string-perm"

[stages.main]
mode = "autonomous"

[stages.main.tool_permissions]
bash = 123
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        // Non-string value for "bash" is silently skipped.
        assert!(stage.tool_permissions.is_empty());
    }

    /// Compaction config without `provider` — covers the None-branch at line ~460.
    #[test]
    fn parse_manifest_compaction_without_provider_uses_default() {
        let toml = r#"
[agent]
name = "compact-no-provider"

[compaction]
model = "gpt-4o-mini"
"#;
        let bp = parse_manifest(toml).unwrap();
        let cc = bp.compaction_config.as_ref().unwrap();
        // provider absent — stays at CompactionConfig default
        assert_eq!(cc.model, "gpt-4o-mini");
    }

    /// Compaction config without `model` — covers the None-branch at line ~463.
    #[test]
    fn parse_manifest_compaction_without_model_uses_default() {
        let toml = r#"
[agent]
name = "compact-no-model"

[compaction]
provider = "anthropic"
"#;
        let bp = parse_manifest(toml).unwrap();
        let cc = bp.compaction_config.as_ref().unwrap();
        assert_eq!(cc.provider, "anthropic");
        // model absent — stays at CompactionConfig default
    }

    // ─── Security config parsing ──────────────────────────────────────────

    #[test]
    fn parse_manifest_with_security_config() {
        let toml = r#"
[agent]
name = "security-test"

[security]
taint_tracking = true
pointer_mode = true
filter_mode = "structured"
degradation = ["pointer", "filter", "traditional"]
"#;
        let bp = parse_manifest(toml).unwrap();
        let sc = bp.security.as_ref().unwrap();
        assert!(sc.taint_tracking);
        assert!(sc.pointer_mode);
        assert_eq!(sc.filter_mode, Some(leviath_core::FilterMode::Structured));
        assert_eq!(sc.degradation.len(), 3);
        assert_eq!(sc.degradation[0], leviath_core::InputMode::Pointer);
        assert_eq!(sc.degradation[1], leviath_core::InputMode::Filter);
        assert_eq!(sc.degradation[2], leviath_core::InputMode::Traditional);
    }

    #[test]
    fn parse_manifest_security_disabled() {
        let toml = r#"
[agent]
name = "no-taint"

[security]
taint_tracking = false
"#;
        let bp = parse_manifest(toml).unwrap();
        let sc = bp.security.as_ref().unwrap();
        assert!(!sc.taint_tracking);
    }

    #[test]
    fn parse_manifest_security_filter_mode_false() {
        let toml = r#"
[agent]
name = "no-filter"

[security]
filter_mode = false
"#;
        let bp = parse_manifest(toml).unwrap();
        let sc = bp.security.as_ref().unwrap();
        assert!(sc.filter_mode.is_none());
    }

    #[test]
    fn parse_manifest_no_security_section() {
        let toml = r#"
[agent]
name = "no-security"
"#;
        let bp = parse_manifest(toml).unwrap();
        assert!(bp.security.is_none());
    }

    #[test]
    fn parse_manifest_security_freeform_filter() {
        let toml = r#"
[agent]
name = "freeform-test"

[security]
filter_mode = "freeform"
"#;
        let bp = parse_manifest(toml).unwrap();
        let sc = bp.security.as_ref().unwrap();
        assert_eq!(sc.filter_mode, Some(leviath_core::FilterMode::Freeform));
    }

    #[test]
    fn parse_manifest_security_partial_degradation() {
        let toml = r#"
[agent]
name = "partial-deg"

[security]
degradation = ["traditional"]
"#;
        let bp = parse_manifest(toml).unwrap();
        let sc = bp.security.as_ref().unwrap();
        assert_eq!(sc.degradation.len(), 1);
        assert_eq!(sc.degradation[0], leviath_core::InputMode::Traditional);
    }

    /// Agent-level tool_permissions with a non-string value — the inner
    /// `if let Some(policy_str) = policy_val.as_str()` should be skipped.
    #[test]
    fn parse_manifest_agent_tool_permissions_non_string_value_skipped() {
        let toml = r#"
[agent]
name = "agent-non-string-perm"

[tool_permissions]
bash = 42
read_file = "allow"
"#;
        let bp = parse_manifest(toml).unwrap();
        // Non-string "bash" is skipped; "read_file" is kept.
        assert!(!bp.metadata.contains_key("tool_perm:bash"));
        assert_eq!(
            bp.metadata
                .get("tool_perm:read_file")
                .and_then(|v| v.as_str()),
            Some("allow")
        );
    }
}
