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
    if let Some(home) = dirs::home_dir() {
        let installed = home
            .join(".leviath")
            .join("agents")
            .join(path)
            .join("agent.leviath");
        if installed.exists() {
            return Ok(installed);
        }
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
        assert!(matches!(start.mode, StageMode::Autonomous));
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
        assert!(matches!(finish.mode, StageMode::Interactive));
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
        assert!(matches!(impl_edge.transform, EdgeTransform::Direct));

        let err_edge = transitions.get("error_handler").unwrap();
        assert_eq!(err_edge.condition, TransitionCondition::Error);
        assert!(matches!(err_edge.transform, EdgeTransform::Clear));

        let timeout_edge = transitions.get("timeout_handler").unwrap();
        assert_eq!(timeout_edge.condition, TransitionCondition::MaxIterations);
        assert!(matches!(
            timeout_edge.transform,
            EdgeTransform::Compact { .. }
        ));

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
        assert!(matches!(sys.kind, RegionKind::Pinned));
        assert_eq!(sys.max_tokens, 1000);

        let conv = bp
            .context_layout
            .regions
            .iter()
            .find(|r| r.name == "conv")
            .unwrap();
        assert!(matches!(
            conv.kind,
            RegionKind::SlidingWindow { max_items: 15 }
        ));

        let temp = bp
            .context_layout
            .regions
            .iter()
            .find(|r| r.name == "temp")
            .unwrap();
        assert!(matches!(temp.kind, RegionKind::Temporary));

        let comp = bp
            .context_layout
            .regions
            .iter()
            .find(|r| r.name == "comp")
            .unwrap();
        assert!(matches!(
            comp.kind,
            RegionKind::Compacting {
                threshold_tokens: 4000
            }
        ));

        let clr = bp
            .context_layout
            .regions
            .iter()
            .find(|r| r.name == "clr")
            .unwrap();
        assert!(matches!(clr.kind, RegionKind::Clearable));

        let hist = bp
            .context_layout
            .regions
            .iter()
            .find(|r| r.name == "hist")
            .unwrap();
        match &hist.kind {
            RegionKind::CompactHistory { source_region } => {
                assert_eq!(source_region, "conv");
            }
            _ => panic!("Expected CompactHistory"),
        }
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
        match &stage.mode {
            StageMode::InteractivePoints { points } => {
                assert_eq!(points.len(), 3);

                assert_eq!(points[0].name, "review");
                assert_eq!(points[0].prompt, "Review the output");
                assert!(points[0].required);
                assert!(matches!(
                    points[0].style,
                    leviath_core::blueprint::InteractionStyle::MultipleChoice
                ));
                assert_eq!(points[0].options, vec!["approve", "reject", "revise"]);

                assert_eq!(points[1].name, "feedback");
                assert!(!points[1].required);
                assert!(matches!(
                    points[1].style,
                    leviath_core::blueprint::InteractionStyle::FreeText
                ));

                assert_eq!(points[2].name, "confirm");
                assert!(matches!(
                    points[2].style,
                    leviath_core::blueprint::InteractionStyle::Confirm
                ));
            }
            _ => panic!("Expected InteractivePoints mode"),
        }
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
        match &stage.mode {
            StageMode::InteractivePoints { points } => {
                assert_eq!(points.len(), 1);
                assert_eq!(
                    points[0].followups.get("Revise").map(|s| s.as_str()),
                    Some("What would you like to change?")
                );
                assert!(!points[0].followups.contains_key("Approve"));
                assert!(!points[0].followups.contains_key("Abort"));
            }
            _ => panic!("Expected InteractivePoints mode"),
        }
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
        match &stage.mode {
            StageMode::InteractivePoints { points } => {
                assert!(points[0].followups.is_empty());
            }
            _ => panic!("Expected InteractivePoints mode"),
        }
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
        match &edge.transform {
            EdgeTransform::Custom {
                carry,
                compact,
                clear,
                compact_prompt,
            } => {
                assert_eq!(carry, &vec!["system"]);
                assert_eq!(compact, &vec!["conversation"]);
                assert_eq!(clear, &vec!["scratch"]);
                assert_eq!(compact_prompt.as_deref(), Some("Summarize for next stage"));
            }
            _ => panic!("Expected Custom transform"),
        }
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
        assert!(matches!(
            bp.context_layout.regions[0].kind,
            RegionKind::Pinned
        ));
        assert_eq!(bp.context_layout.regions[1].name, "conversation");
        assert!(matches!(
            bp.context_layout.regions[1].kind,
            RegionKind::SlidingWindow { .. }
        ));
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
        assert!(matches!(region.kind, RegionKind::Temporary));
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
        assert!(matches!(auto.mode, StageMode::Autonomous));

        let inter = bp.find_stage("inter").unwrap();
        assert!(matches!(inter.mode, StageMode::Interactive));

        // Default mode (no mode specified) — Autonomous
        let default = bp.find_stage("default_mode").unwrap();
        assert!(matches!(default.mode, StageMode::Autonomous));
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
        assert!(matches!(edge.transform, EdgeTransform::Compact { .. }));
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
        let result = find_manifest("/nonexistent/path/to/nothing");
        assert!(result.is_err());
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
        assert!(
            transitions.len() >= 2,
            "plan stage must have >=2 outgoing edges so the user's plan_approval \
             choice (Revise/Add detail/Abort) actually changes behavior instead \
             of being silently ignored by a single-edge auto-transition; got {:?}",
            transitions.keys().collect::<Vec<_>>()
        );
        assert!(transitions.contains_key("implement"));

        // A self-loop (or other non-"implement" edge) must exist so revising/
        // aborting doesn't fall through to implementation.
        assert!(
            transitions.keys().any(|t| t != "implement"),
            "plan stage needs an edge other than 'implement' for non-approval choices"
        );

        // The self-loop must be revisit-capped to avoid an infinite planning loop.
        if transitions.contains_key("plan") {
            assert!(
                plan.max_revisits.is_some(),
                "self-looping 'plan' stage must cap max_revisits"
            );
        }
    }

    #[test]
    fn software_engineer_plan_stage_has_error_edge_and_can_abort() {
        let manifest_content =
            include_str!("../../../../../agents/software-engineer/agent.leviath");
        let bp = parse_manifest(manifest_content).unwrap();
        let plan = bp.find_stage("plan").unwrap();
        let transitions = plan.transitions.as_ref().unwrap();

        assert!(
            transitions
                .get("error_recovery")
                .map(|e| e.condition == leviath_core::blueprint::TransitionCondition::Error)
                .unwrap_or(false),
            "plan stage should route errors to error_recovery, like implement/review do"
        );

        // allow_complete lets the model respond DONE (e.g. when the user
        // chose "Abort") instead of being forced into 'implement' or 'plan'.
        assert!(
            plan.allow_complete,
            "plan stage must allow_complete so 'Abort' can actually end the run"
        );
    }

    #[test]
    fn software_engineer_plan_approval_has_followups_for_non_terminal_choices() {
        let manifest_content =
            include_str!("../../../../../agents/software-engineer/agent.leviath");
        let bp = parse_manifest(manifest_content).unwrap();
        let plan = bp.find_stage("plan").unwrap();
        let points = match &plan.mode {
            StageMode::InteractivePoints { points } => points,
            _ => panic!("plan stage should be interactive_points"),
        };
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
        assert!(
            approval.followups.contains_key(revise_key),
            "Revise option must have a followup prompt asking what to change"
        );
        assert!(
            approval.followups.contains_key(detail_key),
            "Add detail option must have a followup prompt asking which section"
        );

        // Approve/Abort are terminal/decisive — they must NOT have followups
        // (no further elaboration needed).
        for opt in &approval.options {
            if opt.starts_with("Approve") || opt.starts_with("Abort") {
                assert!(
                    !approval.followups.contains_key(opt),
                    "terminal option '{}' should not have a followup",
                    opt
                );
            }
        }
    }

    #[test]
    fn software_engineer_review_stage_can_complete_and_routes_errors() {
        let manifest_content =
            include_str!("../../../../../agents/software-engineer/agent.leviath");
        let bp = parse_manifest(manifest_content).unwrap();
        let review = bp.find_stage("review").unwrap();

        assert!(
            review.allow_complete,
            "review stage must allow_complete — an approving review has no \
             real next stage and must not be forced back into 'implement'"
        );

        let transitions = review
            .transitions
            .as_ref()
            .expect("review stage must declare transitions");
        assert!(
            transitions
                .get("error_recovery")
                .map(|e| e.condition == leviath_core::blueprint::TransitionCondition::Error)
                .unwrap_or(false),
            "review stage should route errors to error_recovery, like implement does"
        );
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
}
