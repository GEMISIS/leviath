//! Manifest parsing for `agent.leviath` files.
//!
//! Pure `TOML` string -> [`Blueprint`] parsing with no filesystem or async
//! dependencies. Filesystem-based manifest discovery (`find_manifest`) lives in
//! `leviath-cli`, since it depends on cli-only path helpers.

use crate::blueprint::{
    EdgeTransform, ModelConfig, ModelEntry, StageMode, TransitionCondition, TransitionEdge,
};
use crate::error::{Error, Result};
use crate::layout::RegionDefinition;
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

    let mut stages = Vec::new();
    if let Some(stages_table) = parsed.get("stages").and_then(|v| v.as_table()) {
        for (stage_name, stage_value) in stages_table {
            let model_table = stage_value.get("model").and_then(|v| v.as_table());
            let model_config = if let Some(mt) = model_table {
                let mut models = Vec::new();

                // New format: [[stages.<name>.model.models]] list
                if let Some(models_arr) = mt.get("models").and_then(|v| v.as_array()) {
                    for entry in models_arr {
                        if let Some(entry_table) = entry.as_table() {
                            models.push(ModelEntry::new(
                                entry_table
                                    .get("provider")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("anthropic")
                                    .to_string(),
                                entry_table
                                    .get("model")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("claude-sonnet-4-6")
                                    .to_string(),
                            ));
                        }
                    }
                }

                // Backward compat: old single-model format (provider + model at
                // top level) or old fallbacks list — treat both as models entries.
                if models.is_empty() {
                    if let Some(provider) = mt.get("provider").and_then(|v| v.as_str()) {
                        let model_name = mt
                            .get("model")
                            .and_then(|v| v.as_str())
                            .unwrap_or("claude-sonnet-4-6");
                        models.push(ModelEntry::new(
                            provider.to_string(),
                            model_name.to_string(),
                        ));
                    }

                    // Old fallbacks become additional models entries
                    if let Some(fallbacks_arr) = mt.get("fallbacks").and_then(|v| v.as_array()) {
                        for fb in fallbacks_arr {
                            if let Some(fb_table) = fb.as_table() {
                                models.push(ModelEntry::new(
                                    fb_table
                                        .get("provider")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("anthropic")
                                        .to_string(),
                                    fb_table
                                        .get("model")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("claude-sonnet-4-6")
                                        .to_string(),
                                ));
                            }
                        }
                    }
                }

                // If still empty, use defaults
                if models.is_empty() {
                    models.push(ModelEntry::new(
                        "anthropic".to_string(),
                        "claude-sonnet-4-6".to_string(),
                    ));
                }

                let allow_user_default = mt
                    .get("allow_user_default")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);

                // Parse parameters
                let mut parameters = std::collections::HashMap::new();
                if let Some(params) = mt.get("parameters").and_then(|v| v.as_table()) {
                    for (k, v) in params {
                        // Converting a parsed `toml::Value` to JSON is infallible:
                        // serde_json maps non-finite floats to null rather than
                        // erroring, and every other toml scalar/collection maps
                        // cleanly.
                        let json_val = serde_json::to_value(v)
                            .expect("infallible: toml::Value always converts to serde_json::Value");
                        parameters.insert(k.clone(), json_val);
                    }
                }

                ModelConfig {
                    models,
                    allow_user_default,
                    parameters,
                }
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
                                        crate::blueprint::InteractionStyle::MultipleChoice
                                    }
                                    Some("confirm") => crate::blueprint::InteractionStyle::Confirm,
                                    _ => crate::blueprint::InteractionStyle::FreeText,
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
                                // Per-option directives, keyed by option label:
                                // [stages.<name>.interaction_points.directives]
                                // "Revise — I'll describe changes" = "Call ask_user_text ..."
                                // `followups` is accepted as a backward-compat alias.
                                let pt_directives: std::collections::HashMap<String, String> = pt
                                    .get("directives")
                                    .or_else(|| pt.get("followups"))
                                    .and_then(|v| v.as_table())
                                    .map(|tbl| {
                                        tbl.iter()
                                            .filter_map(|(k, v)| {
                                                v.as_str().map(|s| (k.clone(), s.to_string()))
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                // Options that immediately abort the run:
                                // abort_options = ["Abort — cancel this run"]
                                let pt_abort_options: Vec<String> = pt
                                    .get("abort_options")
                                    .and_then(|v| v.as_array())
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                // Options that open the last output for direct editing:
                                // edit_options = ["Add detail — expand a section"]
                                let pt_edit_options: Vec<String> = pt
                                    .get("edit_options")
                                    .and_then(|v| v.as_array())
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                // Pinned region that holds the authoritative
                                // document: document_region = "plan"
                                let pt_document_region: Option<String> = pt
                                    .get("document_region")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                points.push(crate::blueprint::InteractionPoint {
                                    name: pt_name,
                                    prompt: pt_prompt,
                                    required: pt_required,
                                    style: pt_style,
                                    options: pt_options,
                                    directives: pt_directives,
                                    abort_options: pt_abort_options,
                                    edit_options: pt_edit_options,
                                    document_region: pt_document_region,
                                });
                            }
                        }
                        stage.with_mode(StageMode::InteractivePoints { points })
                    }
                    "fan_out" => {
                        let str_field = |key: &str| {
                            stage_value
                                .get(key)
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        };
                        let on_worker_failure = match stage_value
                            .get("on_worker_failure")
                            .and_then(|v| v.as_str())
                        {
                            Some("fail_all") => crate::blueprint::WorkerFailurePolicy::FailAll,
                            // "continue" / missing / unknown all mean continue.
                            _ => crate::blueprint::WorkerFailurePolicy::Continue,
                        };
                        let config = crate::blueprint::FanOutConfig {
                            worker_agent: str_field("worker_agent"),
                            worker_stage: str_field("worker_stage"),
                            worker_query: str_field("worker_query"),
                            merge_stage: str_field("merge_stage"),
                            max_workers: stage_value
                                .get("max_workers")
                                .and_then(|v| v.as_integer())
                                .map(|n| n as usize)
                                .unwrap_or(4),
                            on_worker_failure,
                            split_prompt: str_field("split_prompt").unwrap_or_default(),
                        };
                        stage.with_mode(StageMode::FanOut { config })
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

            // Warn on a common authoring mistake: a `system_prompt` written
            // *after* the `[stages.X.model]` sub-table lands under
            // `stages.X.model` (TOML nesting rules) and is silently ignored, so
            // the stage runs with no instructions. Point the author at the fix.
            let model_has_system_prompt = stage_value
                .get("model")
                .and_then(|v| v.as_table())
                .map(|t| t.contains_key("system_prompt"))
                .unwrap_or(false);
            if model_has_system_prompt {
                tracing::warn!(
                    "stage '{stage_name}': `system_prompt` is nested under \
                     [stages.{stage_name}.model] and will be IGNORED — move the \
                     `system_prompt = \"\"\"...\"\"\"` line ABOVE the \
                     [stages.{stage_name}.model] table so it belongs to the stage"
                );
            }

            // Parse tool_routing configuration
            if let Some(routing_table) = stage_value.get("tool_routing").and_then(|v| v.as_table())
            {
                let mut routing = crate::blueprint::ToolResultRouting::default();

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

            // Parse allow_as_worker flag: opts this stage in to being used as a
            // fan-out `worker_stage` target.
            if let Some(aw) = stage_value.get("allow_as_worker").and_then(|v| v.as_bool()) {
                stage.allow_as_worker = aw;
            }

            // Parse per-stage security override: [stages.<name>.security]
            if let Some(sec_table) = stage_value.get("security").and_then(|v| v.as_table()) {
                stage.security = Some(parse_security_config(sec_table));
            }

            // Parse accepts_messages flag: whether mid-run user messages are
            // injected into context between inference calls. Defaults to true
            // (via the Stage constructor); set false for stages that shouldn't
            // be interrupted (e.g. a final report generation stage).
            if let Some(am) = stage_value
                .get("accepts_messages")
                .and_then(|v| v.as_bool())
            {
                stage.accepts_messages = am;
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
                    let eviction_strategy =
                        match region_value.get("strategy").and_then(|v| v.as_str()) {
                            Some("bulk") => {
                                let overflow = region_value
                                    .get("overflow")
                                    .and_then(|v| v.as_integer())
                                    .unwrap_or(10)
                                    as usize;
                                EvictionStrategy::Bulk { overflow }
                            }
                            Some("compact") => {
                                let compact_count = region_value
                                    .get("compact_count")
                                    .and_then(|v| v.as_integer())
                                    .unwrap_or(10)
                                    as usize;
                                EvictionStrategy::Compact { compact_count }
                            }
                            _ => EvictionStrategy::PerItem,
                        };
                    RegionKind::SlidingWindow {
                        max_items,
                        eviction_strategy,
                    }
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
                "hashmap" | "hash_map" => {
                    let max_entries = region_value
                        .get("max_entries")
                        .and_then(|v| v.as_integer())
                        .map(|v| v as usize);
                    RegionKind::HashMap { max_entries }
                }
                _ => RegionKind::Temporary,
            };

            let required = region_value
                .get("required")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let required_message = region_value
                .get("required_message")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            total_tokens += max_tokens;
            regions.push(
                RegionDefinition::new(region_name.clone(), kind, max_tokens)
                    .with_required(required, required_message),
            );
        }
    }

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

    // Parse agent-level security config: [security]
    if let Some(security_table) = parsed.get("security").and_then(|v| v.as_table()) {
        blueprint.security = Some(parse_security_config(security_table));
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

    // Parse file tracking config: [context.file_tracking]
    if let Some(context_table) = parsed.get("context").and_then(|v| v.as_table())
        && let Some(ft_table) = context_table
            .get("file_tracking")
            .and_then(|v| v.as_table())
    {
        let region = ft_table
            .get("region")
            .and_then(|v| v.as_str())
            .unwrap_or("files")
            .to_string();
        let track_reads = ft_table
            .get("track_reads")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let track_writes = ft_table
            .get("track_writes")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let max_file_tokens = ft_table
            .get("max_file_tokens")
            .and_then(|v| v.as_integer())
            .map(|v| v as usize);

        blueprint.file_tracking = Some(crate::FileTrackingConfig {
            region,
            track_reads,
            track_writes,
            max_file_tokens,
        });
    }

    Ok(blueprint)
}

/// Parse a `[security]` / `[stages.X.security]` table into a `SecurityConfig`.
/// A present block defaults `taint_tracking` to `true` (block presence implies
/// intent to configure security); omit the block entirely to inherit the
/// broader (agent/global) setting.
fn parse_security_config(security_table: &toml::value::Table) -> crate::SecurityConfig {
    let mut sc = crate::SecurityConfig::default();
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
            sc.filter_mode = crate::FilterMode::from_str_loose(fm_str);
        } else if let Some(false) = fm_val.as_bool() {
            sc.filter_mode = None;
        }
    }
    if let Some(deg_arr) = security_table.get("degradation").and_then(|v| v.as_array()) {
        let modes: Vec<crate::InputMode> = deg_arr
            .iter()
            .filter_map(|v| v.as_str().and_then(crate::InputMode::from_str_loose))
            .collect();
        if !modes.is_empty() {
            sc.degradation = modes;
        }
    }
    sc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extract the `points` vec from a `StageMode::InteractivePoints`.
    /// Panics (with a diagnostic) when the mode is any other variant.
    /// The panic branch is exercised by `unwrap_interactive_points_panics_on_wrong_mode`.
    fn unwrap_interactive_points(mode: &StageMode) -> &[crate::blueprint::InteractionPoint] {
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
        assert_eq!(start.model.provider(), "openai");
        assert_eq!(start.model.model(), "gpt-5");
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
        assert_eq!(
            conv.kind,
            RegionKind::SlidingWindow {
                max_items: 15,
                eviction_strategy: EvictionStrategy::PerItem,
            }
        );

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
    fn parse_manifest_with_model_config() {
        let toml = r#"
[agent]
name = "model-test"

[stages.main]
model = { provider = "google", model = "gemini-3.5-pro" }
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        assert_eq!(stage.model.provider(), "google");
        assert_eq!(stage.model.model(), "gemini-3.5-pro");
    }

    #[test]
    fn system_prompt_nested_under_model_is_ignored_and_warned() {
        // A `system_prompt` written after the `[stages.main.model]` table nests
        // under the model table (TOML rules), so the stage never receives it.
        // parse_manifest emits a warning; the stage config must NOT contain it.
        let toml = r#"
[agent]
name = "misplaced-sp"

[stages.main]
mode = "autonomous"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"
system_prompt = "these instructions are misplaced under [model]"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        assert!(
            !stage.config.contains_key("system_prompt"),
            "a system_prompt nested under [model] must not become the stage prompt"
        );
    }

    #[test]
    fn parse_manifest_reads_region_required_and_message() {
        let toml = r#"
[agent]
name = "req-test"

[stages.main]
mode = "autonomous"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[context.regions]
plan = { kind = "pinned", max_tokens = 4000, required = true, required_message = "write the plan" }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#;
        let bp = parse_manifest(toml).unwrap();
        let plan = bp.context_layout.get_region("plan").unwrap();
        assert!(plan.required, "required flag parsed");
        assert_eq!(plan.required_message.as_deref(), Some("write the plan"));
        let conv = bp.context_layout.get_region("conversation").unwrap();
        assert!(!conv.required, "unmarked region defaults to not required");
        assert!(conv.required_message.is_none());
    }

    #[test]
    fn parse_manifest_model_with_models_list() {
        let toml = r#"
[agent]
name = "models-list-test"

[stages.main.model]
allow_user_default = false

[[stages.main.model.models]]
provider = "anthropic"
model = "claude-sonnet-4-6"

[[stages.main.model.models]]
provider = "openai"
model = "gpt-4o"

[[stages.main.model.models]]
provider = "ollama"
model = "llama3"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        assert_eq!(stage.model.models.len(), 3);
        assert_eq!(stage.model.models[0].provider, "anthropic");
        assert_eq!(stage.model.models[0].model, "claude-sonnet-4-6");
        assert_eq!(stage.model.models[1].provider, "openai");
        assert_eq!(stage.model.models[1].model, "gpt-4o");
        assert_eq!(stage.model.models[2].provider, "ollama");
        assert_eq!(stage.model.models[2].model, "llama3");
        assert!(!stage.model.allow_user_default);
    }

    #[test]
    fn parse_manifest_model_backward_compat_fallbacks() {
        // Old format with fallbacks should be converted to models list
        let toml = r#"
[agent]
name = "fallback-test"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-4-6"

[[stages.main.model.fallbacks]]
provider = "openai"
model = "gpt-4o"

[[stages.main.model.fallbacks]]
provider = "ollama"
model = "llama3"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        assert_eq!(stage.model.models.len(), 3);
        assert_eq!(stage.model.models[0].provider, "anthropic");
        assert_eq!(stage.model.models[0].model, "claude-sonnet-4-6");
        assert_eq!(stage.model.models[1].provider, "openai");
        assert_eq!(stage.model.models[1].model, "gpt-4o");
        assert_eq!(stage.model.models[2].provider, "ollama");
        assert_eq!(stage.model.models[2].model, "llama3");
    }

    #[test]
    fn parse_manifest_model_with_parameters() {
        let toml = r#"
[agent]
name = "params-test"

[stages.main]

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-4-6"

[stages.main.model.parameters]
temperature = 0.3
max_output_tokens = 8192
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        assert_eq!(
            stage
                .model
                .parameters
                .get("temperature")
                .and_then(|v| v.as_f64()),
            Some(0.3)
        );
        assert_eq!(
            stage
                .model
                .parameters
                .get("max_output_tokens")
                .and_then(|v| v.as_u64()),
            Some(8192)
        );
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
        assert_eq!(stage.model.provider(), "anthropic");
        assert_eq!(stage.model.model(), "claude-sonnet-4-6");
    }

    #[test]
    fn parse_manifest_model_table_without_models_uses_default() {
        // A model table that exists but declares no `models`, no top-level
        // `provider`, and no `fallbacks` must fall through to the built-in
        // default single entry.
        let toml = r#"
[agent]
name = "empty-model-table"

[stages.main.model]
allow_user_default = false
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        assert_eq!(stage.model.models.len(), 1);
        assert_eq!(stage.model.models[0].provider, "anthropic");
        assert_eq!(stage.model.models[0].model, "claude-sonnet-4-6");
        assert!(!stage.model.allow_user_default);
    }

    #[test]
    fn parse_manifest_models_array_skips_non_table_and_applies_defaults() {
        // A non-table entry in the `models` array is skipped; table entries
        // missing `provider`/`model` fall back to the per-field defaults.
        let toml = r#"
[agent]
name = "models-defaults"

[stages.main.model]
models = ["skip-me", { provider = "openai" }, { model = "custom-model" }]
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        assert_eq!(stage.model.models.len(), 2);
        // provider given, model defaulted
        assert_eq!(stage.model.models[0].provider, "openai");
        assert_eq!(stage.model.models[0].model, "claude-sonnet-4-6");
        // model given, provider defaulted
        assert_eq!(stage.model.models[1].provider, "anthropic");
        assert_eq!(stage.model.models[1].model, "custom-model");
    }

    #[test]
    fn parse_manifest_top_level_provider_without_model() {
        // Old single-model format with a top-level provider but no model →
        // model defaults to claude-sonnet-4-6.
        let toml = r#"
[agent]
name = "provider-only"

[stages.main.model]
provider = "openai"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        assert_eq!(stage.model.models.len(), 1);
        assert_eq!(stage.model.models[0].provider, "openai");
        assert_eq!(stage.model.models[0].model, "claude-sonnet-4-6");
    }

    #[test]
    fn parse_manifest_fallbacks_without_top_level_provider() {
        // `fallbacks` with no top-level provider: non-table entries are
        // skipped and per-field defaults apply to the table entries.
        let toml = r#"
[agent]
name = "fallbacks-only"

[stages.main.model]
fallbacks = ["skip-me", { provider = "openai" }, { model = "custom-model" }]
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        assert_eq!(stage.model.models.len(), 2);
        // provider given, model defaulted
        assert_eq!(stage.model.models[0].provider, "openai");
        assert_eq!(stage.model.models[0].model, "claude-sonnet-4-6");
        // model given, provider defaulted
        assert_eq!(stage.model.models[1].provider, "anthropic");
        assert_eq!(stage.model.models[1].model, "custom-model");
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
document_region = "plan"

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
            crate::blueprint::InteractionStyle::MultipleChoice
        );
        assert_eq!(points[0].options, vec!["approve", "reject", "revise"]);
        assert_eq!(points[0].document_region.as_deref(), Some("plan"));
        // A point that omits it parses to None.
        assert_eq!(points[1].document_region, None);

        assert_eq!(points[1].name, "feedback");
        assert!(!points[1].required);
        assert_eq!(
            points[1].style,
            crate::blueprint::InteractionStyle::FreeText
        );

        assert_eq!(points[2].name, "confirm");
        assert_eq!(points[2].style, crate::blueprint::InteractionStyle::Confirm);
    }

    #[test]
    fn parse_manifest_interaction_point_directives_and_abort() {
        let toml = r#"
[agent]
name = "directive-test"

[stages.plan]
mode = "interactive_points"

[[stages.plan.interaction_points]]
name     = "plan_approval"
prompt   = "Approve?"
required = true
style    = "multiple_choice"
options  = ["Approve", "Revise", "Edit", "Abort"]
abort_options = ["Abort"]
edit_options = ["Edit"]
directives = { "Revise" = "Call ask_user_text to find out what to change." }
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("plan").unwrap();
        let points = unwrap_interactive_points(&stage.mode);
        assert_eq!(points.len(), 1);
        assert_eq!(
            points[0].directives.get("Revise").map(|s| s.as_str()),
            Some("Call ask_user_text to find out what to change.")
        );
        assert!(!points[0].directives.contains_key("Approve"));
        assert_eq!(points[0].abort_options, vec!["Abort".to_string()]);
        assert_eq!(points[0].edit_options, vec!["Edit".to_string()]);
    }

    #[test]
    fn parse_manifest_interaction_point_followups_alias_maps_to_directives() {
        // Backward compat: the old `followups` key is accepted as an alias.
        let toml = r#"
[agent]
name = "followup-alias-test"

[stages.plan]
mode = "interactive_points"

[[stages.plan.interaction_points]]
name     = "plan_approval"
prompt   = "Approve?"
required = true
style    = "multiple_choice"
options  = ["Approve", "Revise"]
followups = { "Revise" = "What would you like to change?" }
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("plan").unwrap();
        let points = unwrap_interactive_points(&stage.mode);
        assert_eq!(
            points[0].directives.get("Revise").map(|s| s.as_str()),
            Some("What would you like to change?")
        );
    }

    #[test]
    fn parse_manifest_agent_and_stage_security() {
        let toml = r#"
[agent]
name = "sec-test"

[security]
taint_tracking = true

[stages.plan]
mode = "autonomous"

[stages.plan.security]
taint_tracking = false

[stages.build]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        // Agent-level [security] parsed.
        assert!(bp.security.as_ref().unwrap().taint_tracking);
        // Stage-level [stages.plan.security] opts this stage out.
        let plan = bp.find_stage("plan").unwrap();
        assert_eq!(
            plan.security.as_ref().map(|s| s.taint_tracking),
            Some(false)
        );
        // A stage with no [security] inherits (None).
        let build = bp.find_stage("build").unwrap();
        assert!(build.security.is_none());
    }

    #[test]
    fn parse_manifest_no_security_is_none() {
        let toml = r#"
[agent]
name = "no-sec"

[stages.main]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        assert!(bp.security.is_none());
        assert!(bp.find_stage("main").unwrap().security.is_none());
    }

    #[test]
    fn parse_manifest_interaction_point_no_directives_defaults_empty() {
        let toml = r#"
[agent]
name = "no-directive-test"

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
        assert!(points[0].directives.is_empty());
        assert!(points[0].abort_options.is_empty());
        assert!(points[0].edit_options.is_empty());
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
    fn parse_manifest_fan_out_stage() {
        let toml = r#"
[agent]
name = "fanout-test"

[stages.parallel]
mode = "fan_out"
worker_stage = "worker"
merge_stage = "merge"
max_workers = 7
on_worker_failure = "fail_all"
split_prompt = "split the work"

[stages.worker]
mode = "autonomous"
allow_as_worker = true

[stages.merge]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        // Compare the whole mode (no never-taken fallback arm to leave uncovered).
        let expected = crate::blueprint::StageMode::FanOut {
            config: crate::blueprint::FanOutConfig {
                worker_agent: None,
                worker_stage: Some("worker".to_string()),
                worker_query: None,
                merge_stage: Some("merge".to_string()),
                max_workers: 7,
                on_worker_failure: crate::blueprint::WorkerFailurePolicy::FailAll,
                split_prompt: "split the work".to_string(),
            },
        };
        assert_eq!(bp.find_stage("parallel").unwrap().mode, expected);
        assert!(bp.find_stage("worker").unwrap().allow_as_worker);
        // Defaults: unspecified fan_out fields.
        assert!(!bp.find_stage("merge").unwrap().allow_as_worker);
    }

    #[test]
    fn parse_manifest_fan_out_defaults() {
        let toml = r#"
[agent]
name = "fanout-defaults"

[stages.parallel]
mode = "fan_out"
worker_agent = "external-worker"
split_prompt = "go"
"#;
        let bp = parse_manifest(toml).unwrap();
        let expected = crate::blueprint::StageMode::FanOut {
            config: crate::blueprint::FanOutConfig {
                worker_agent: Some("external-worker".to_string()),
                worker_stage: None,
                worker_query: None,
                merge_stage: None,
                max_workers: 4, // default
                on_worker_failure: crate::blueprint::WorkerFailurePolicy::Continue,
                split_prompt: "go".to_string(),
            },
        };
        assert_eq!(bp.find_stage("parallel").unwrap().mode, expected);
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
    fn parse_manifest_stage_accepts_messages_false() {
        let toml = r#"
[agent]
name = "accepts-messages-test"

[stages.report]
mode = "autonomous"
accepts_messages = false
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("report").unwrap();
        assert!(!stage.accepts_messages);
    }

    #[test]
    fn parse_manifest_stage_accepts_messages_defaults_true() {
        let toml = r#"
[agent]
name = "accepts-messages-default-test"

[stages.report]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("report").unwrap();
        assert!(stage.accepts_messages);
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
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Missing [agent] section")
        );
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
            RegionKind::SlidingWindow {
                max_items: 10,
                eviction_strategy: EvictionStrategy::default(),
            }
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
        let manifest_content = include_str!("../../../agents/software-engineer/agent.leviath");
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
        let manifest_content = include_str!("../../../agents/software-engineer/agent.leviath");
        let bp = parse_manifest(manifest_content).unwrap();
        let plan = bp.find_stage("plan").unwrap();
        let transitions = plan.transitions.as_ref().unwrap();

        // plan stage should route errors to error_recovery, like implement/review do.
        assert!(
            transitions
                .get("error_recovery")
                .map(|e| e.condition == crate::blueprint::TransitionCondition::Error)
                .unwrap_or(false)
        );

        // allow_complete lets the model respond DONE (e.g. when the user
        // chose "Abort") instead of being forced into 'implement' or 'plan'.
        assert!(plan.allow_complete);
    }

    #[test]
    fn software_engineer_plan_approval_option_routing() {
        let manifest_content = include_str!("../../../agents/software-engineer/agent.leviath");
        let bp = parse_manifest(manifest_content).unwrap();
        let plan = bp.find_stage("plan").unwrap();
        let points = unwrap_interactive_points(&plan.mode);
        let approval = points
            .iter()
            .find(|p| p.name == "plan_approval")
            .expect("plan_approval interaction point must exist");

        let opt = |prefix: &str| {
            approval
                .options
                .iter()
                .find(|o| o.starts_with(prefix))
                .expect("interaction-point option with the requested prefix must exist")
                .clone()
        };
        let approve = opt("Approve");
        let revise = opt("Revise");
        let detail = opt("Add detail");
        let abort = opt("Abort");

        // "Revise" carries a directive (agent-driven, calls ask_user_text).
        assert!(approval.directives.contains_key(&revise));
        // "Add detail" is a deterministic edit option (engine opens an editor).
        assert!(approval.edit_options.contains(&detail));
        assert!(!approval.directives.contains_key(&detail));
        // "Abort" is a deterministic abort option.
        assert!(approval.abort_options.contains(&abort));
        // "Approve" is a plain completing option — none of the above.
        assert!(!approval.directives.contains_key(&approve));
        assert!(!approval.abort_options.contains(&approve));
        assert!(!approval.edit_options.contains(&approve));
    }

    #[test]
    fn software_engineer_review_stage_can_complete_and_routes_errors() {
        let manifest_content = include_str!("../../../agents/software-engineer/agent.leviath");
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
        assert!(
            transitions
                .get("error_recovery")
                .map(|e| e.condition == crate::blueprint::TransitionCondition::Error)
                .unwrap_or(false)
        );
    }

    #[test]
    fn software_engineer_blueprint_passes_full_validation() {
        let manifest_content = include_str!("../../../agents/software-engineer/agent.leviath");
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
        let manifest_content = include_str!("../../../agents/software-engineer/agent.leviath");
        let bp = parse_manifest(manifest_content).unwrap();

        let plan = bp.find_stage("plan").unwrap();
        assert!(plan.available_tools.contains(&"ask_user_text".to_string()));
        assert!(
            plan.available_tools
                .contains(&"ask_user_choice".to_string())
        );

        let implement = bp.find_stage("implement").unwrap();
        assert!(
            implement
                .available_tools
                .contains(&"ask_user_text".to_string())
        );
        assert!(
            implement
                .available_tools
                .contains(&"ask_user_confirm".to_string())
        );
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
        assert_eq!(sc.filter_mode, Some(crate::FilterMode::Structured));
        assert_eq!(sc.degradation.len(), 3);
        assert_eq!(sc.degradation[0], crate::InputMode::Pointer);
        assert_eq!(sc.degradation[1], crate::InputMode::Filter);
        assert_eq!(sc.degradation[2], crate::InputMode::Traditional);
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
        assert_eq!(sc.filter_mode, Some(crate::FilterMode::Freeform));
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
        assert_eq!(sc.degradation[0], crate::InputMode::Traditional);
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

    // ─── Models list & allow_user_default tests ─────────────────────────────

    #[test]
    fn parse_manifest_models_list_priority_order() {
        let toml = r#"
[agent]
name = "priority-test"

[stages.main.model]
allow_user_default = true

[[stages.main.model.models]]
provider = "anthropic"
model = "claude-sonnet-4-6"

[[stages.main.model.models]]
provider = "openai"
model = "gpt-4o"

[[stages.main.model.models]]
provider = "ollama"
model = "llama3"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        assert_eq!(stage.model.models.len(), 3);
        // Order preserved
        assert_eq!(stage.model.models[0].provider, "anthropic");
        assert_eq!(stage.model.models[0].model, "claude-sonnet-4-6");
        assert_eq!(stage.model.models[1].provider, "openai");
        assert_eq!(stage.model.models[1].model, "gpt-4o");
        assert_eq!(stage.model.models[2].provider, "ollama");
        assert_eq!(stage.model.models[2].model, "llama3");
        assert!(stage.model.allow_user_default);
    }

    #[test]
    fn parse_manifest_allow_user_default_false() {
        let toml = r#"
[agent]
name = "no-fallback-test"

[stages.main.model]
allow_user_default = false

[[stages.main.model.models]]
provider = "anthropic"
model = "claude-sonnet-4-6"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        assert!(!stage.model.allow_user_default);
        assert_eq!(stage.model.models.len(), 1);
    }

    #[test]
    fn parse_manifest_allow_user_default_defaults_true() {
        let toml = r#"
[agent]
name = "default-aud-test"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-4-6"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        assert!(stage.model.allow_user_default);
    }

    #[test]
    fn parse_manifest_backward_compat_single_model_inline() {
        // Old inline format: model = { provider = "...", model = "..." }
        let toml = r#"
[agent]
name = "compat-test"

[stages.main]
model = { provider = "google", model = "gemini-3.5-pro" }
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        assert_eq!(stage.model.models.len(), 1);
        assert_eq!(stage.model.models[0].provider, "google");
        assert_eq!(stage.model.models[0].model, "gemini-3.5-pro");
        assert!(stage.model.allow_user_default);
    }

    #[test]
    fn parse_manifest_models_list_with_parameters() {
        let toml = r#"
[agent]
name = "params-models-test"

[stages.main.model]
allow_user_default = true

[stages.main.model.parameters]
temperature = 0.3
max_output_tokens = 16384

[[stages.main.model.models]]
provider = "anthropic"
model = "claude-sonnet-4-6"

[[stages.main.model.models]]
provider = "openai"
model = "gpt-4o"
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        assert_eq!(stage.model.models.len(), 2);
        assert_eq!(
            stage
                .model
                .parameters
                .get("temperature")
                .and_then(|v| v.as_f64()),
            Some(0.3)
        );
        assert_eq!(
            stage
                .model
                .parameters
                .get("max_output_tokens")
                .and_then(|v| v.as_u64()),
            Some(16384)
        );
    }

    #[test]
    fn parse_manifest_max_output_tokens_override_via_parameters() {
        let toml = r#"
[agent]
name = "token-override-test"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-4-6"

[stages.main.model.parameters]
max_output_tokens = 2048
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        assert_eq!(
            stage
                .model
                .parameters
                .get("max_output_tokens")
                .and_then(|v| v.as_u64()),
            Some(2048)
        );
    }

    #[test]
    fn parse_manifest_with_hashmap_region() {
        let toml = r#"
[agent]
name = "test"

[context.regions]
files = { kind = "hashmap", max_tokens = 40000 }
files_limited = { kind = "hashmap", max_tokens = 20000, max_entries = 50 }
"#;
        let bp = parse_manifest(toml).unwrap();
        let files_region = bp.context_layout.get_region("files").unwrap();
        assert_eq!(files_region.kind, RegionKind::HashMap { max_entries: None });
        assert_eq!(files_region.max_tokens, 40000);

        let limited = bp.context_layout.get_region("files_limited").unwrap();
        assert_eq!(
            limited.kind,
            RegionKind::HashMap {
                max_entries: Some(50)
            }
        );
    }

    #[test]
    fn parse_manifest_with_file_tracking() {
        let toml = r#"
[agent]
name = "test"

[context.regions]
files = { kind = "hashmap", max_tokens = 40000 }

[context.file_tracking]
region = "files"
track_reads = true
track_writes = true
max_file_tokens = 5000
"#;
        let bp = parse_manifest(toml).unwrap();
        let ft = bp.file_tracking.unwrap();
        assert_eq!(ft.region, "files");
        assert!(ft.track_reads);
        assert!(ft.track_writes);
        assert_eq!(ft.max_file_tokens, Some(5000));
    }

    #[test]
    fn parse_manifest_file_tracking_defaults() {
        let toml = r#"
[agent]
name = "test"

[context.file_tracking]
region = "myfiles"
"#;
        let bp = parse_manifest(toml).unwrap();
        let ft = bp.file_tracking.unwrap();
        assert_eq!(ft.region, "myfiles");
        assert!(ft.track_reads);
        assert!(ft.track_writes);
        assert!(ft.max_file_tokens.is_none());
    }

    // ─── tool_routing ────────────────────────────────────────────────────────

    #[test]
    fn parse_stage_tool_routing_all_fields() {
        let toml = r#"
[agent]
name = "routing-test"

[stages.main]
mode = "autonomous"

[stages.main.tool_routing]
default_region = "my_results"
persist = true
max_result_tokens = 4096

[stages.main.tool_routing.overrides]
bash = "bash_output"
read_file = "file_contents"
"#;
        let bp = parse_manifest(toml).unwrap();
        let main = bp.find_stage("main").unwrap();
        let routing = main
            .tool_result_routing
            .as_ref()
            .expect("tool_result_routing should be Some");
        assert_eq!(routing.default_region, "my_results");
        assert!(routing.persist);
        assert_eq!(routing.max_result_tokens, Some(4096));
        assert_eq!(routing.tool_overrides.len(), 2);
        assert_eq!(routing.tool_overrides.get("bash").unwrap(), "bash_output");
        assert_eq!(
            routing.tool_overrides.get("read_file").unwrap(),
            "file_contents"
        );
    }

    #[test]
    fn parse_stage_tool_routing_only_default_region() {
        let toml = r#"
[agent]
name = "routing-partial"

[stages.main]
mode = "autonomous"

[stages.main.tool_routing]
default_region = "custom_region"
"#;
        let bp = parse_manifest(toml).unwrap();
        let main = bp.find_stage("main").unwrap();
        let routing = main
            .tool_result_routing
            .as_ref()
            .expect("tool_result_routing should be Some");
        assert_eq!(routing.default_region, "custom_region");
        // defaults from ToolResultRouting::default()
        assert!(routing.persist);
        assert!(routing.max_result_tokens.is_none());
        assert!(routing.tool_overrides.is_empty());
    }

    #[test]
    fn parse_stage_without_tool_routing() {
        let toml = r#"
[agent]
name = "no-routing"

[stages.main]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        let main = bp.find_stage("main").unwrap();
        assert!(main.tool_result_routing.is_none());
    }

    #[test]
    fn parse_stage_tool_routing_with_overrides_only() {
        let toml = r#"
[agent]
name = "overrides-only"

[stages.main]
mode = "autonomous"

[stages.main.tool_routing]

[stages.main.tool_routing.overrides]
search = "search_results"
write_file = "written_files"
compile = "build_output"
"#;
        let bp = parse_manifest(toml).unwrap();
        let main = bp.find_stage("main").unwrap();
        let routing = main
            .tool_result_routing
            .as_ref()
            .expect("tool_result_routing should be Some");
        // default_region keeps its default since we didn't set it
        assert_eq!(routing.default_region, "tool_results");
        assert_eq!(routing.tool_overrides.len(), 3);
        assert_eq!(
            routing.tool_overrides.get("search").unwrap(),
            "search_results"
        );
        assert_eq!(
            routing.tool_overrides.get("write_file").unwrap(),
            "written_files"
        );
        assert_eq!(
            routing.tool_overrides.get("compile").unwrap(),
            "build_output"
        );
    }

    #[test]
    fn parse_stage_tool_routing_persist_false() {
        let toml = r#"
[agent]
name = "persist-false"

[stages.main]
mode = "autonomous"

[stages.main.tool_routing]
persist = false
"#;
        let bp = parse_manifest(toml).unwrap();
        let main = bp.find_stage("main").unwrap();
        let routing = main
            .tool_result_routing
            .as_ref()
            .expect("tool_result_routing should be Some");
        assert!(!routing.persist);
        // other fields keep defaults
        assert_eq!(routing.default_region, "tool_results");
        assert!(routing.max_result_tokens.is_none());
        assert!(routing.tool_overrides.is_empty());
    }

    #[test]
    fn parse_stage_tool_routing_max_result_tokens() {
        let toml = r#"
[agent]
name = "max-tokens"

[stages.main]
mode = "autonomous"

[stages.main.tool_routing]
max_result_tokens = 8192
"#;
        let bp = parse_manifest(toml).unwrap();
        let main = bp.find_stage("main").unwrap();
        let routing = main
            .tool_result_routing
            .as_ref()
            .expect("tool_result_routing should be Some");
        assert_eq!(routing.max_result_tokens, Some(8192));
        // other fields keep defaults
        assert_eq!(routing.default_region, "tool_results");
        assert!(routing.persist);
        assert!(routing.tool_overrides.is_empty());
    }

    #[test]
    fn parse_manifest_sliding_window_bulk_and_compact_strategies() {
        // Exercises the `bulk` and `compact` eviction-strategy arms of the
        // sliding_window region parser (with and without their optional counts).
        let toml = r#"
[agent]
name = "eviction-strategies"

[context.regions]
bulk_default = { kind = "sliding_window", max_items = 20, max_tokens = 3000, strategy = "bulk" }
bulk_overflow = { kind = "sliding_window", max_items = 20, max_tokens = 3000, strategy = "bulk", overflow = 5 }
compact_default = { kind = "sliding_window", max_items = 20, max_tokens = 3000, strategy = "compact" }
compact_count = { kind = "sliding_window", max_items = 20, max_tokens = 3000, strategy = "compact", compact_count = 7 }
"#;
        let bp = parse_manifest(toml).unwrap();
        let region = |name: &str| {
            bp.context_layout
                .regions
                .iter()
                .find(|r| r.name == name)
                .unwrap()
                .kind
                .clone()
        };

        assert_eq!(
            region("bulk_default"),
            RegionKind::SlidingWindow {
                max_items: 20,
                eviction_strategy: EvictionStrategy::Bulk { overflow: 10 },
            }
        );
        assert_eq!(
            region("bulk_overflow"),
            RegionKind::SlidingWindow {
                max_items: 20,
                eviction_strategy: EvictionStrategy::Bulk { overflow: 5 },
            }
        );
        assert_eq!(
            region("compact_default"),
            RegionKind::SlidingWindow {
                max_items: 20,
                eviction_strategy: EvictionStrategy::Compact { compact_count: 10 },
            }
        );
        assert_eq!(
            region("compact_count"),
            RegionKind::SlidingWindow {
                max_items: 20,
                eviction_strategy: EvictionStrategy::Compact { compact_count: 7 },
            }
        );
    }

    #[test]
    fn parse_manifest_tool_routing_override_non_string_value_is_skipped() {
        // A non-string override value fails `region_val.as_str()` and is
        // skipped, exercising the `if let Some(region_name)` false path; the
        // string-valued override is still inserted.
        let toml = r#"
[agent]
name = "routing-nonstring"

[stages.main]
mode = "autonomous"

[stages.main.tool_routing]
default_region = "temp"

[stages.main.tool_routing.overrides]
read_file = "files"
write_file = 123
"#;
        let bp = parse_manifest(toml).unwrap();
        let stage = bp.find_stage("main").unwrap();
        let routing = stage.tool_result_routing.as_ref().unwrap();
        assert_eq!(
            routing.tool_overrides.get("read_file").map(|s| s.as_str()),
            Some("files")
        );
        assert!(!routing.tool_overrides.contains_key("write_file"));
    }

    #[test]
    fn parse_manifest_security_all_branches() {
        // Agent-level [security]: pointer_mode, filter_mode = false (→ None),
        // and a non-empty degradation list. Stage-level [security]: a string
        // filter_mode. Together these exercise every branch of
        // parse_security_config.
        let toml = r#"
[agent]
name = "sec-all-branches"

[security]
taint_tracking = false
pointer_mode = true
filter_mode = false
degradation = ["pointer", "filter", "not-a-real-mode"]

[stages.a]
mode = "autonomous"

[stages.a.security]
filter_mode = "structured"

[stages.b]
mode = "autonomous"

[stages.c]
mode = "autonomous"

[stages.c.security]
filter_mode = true
"#;
        let bp = parse_manifest(toml).unwrap();
        let agent_sec = bp.security.as_ref().unwrap();
        assert!(!agent_sec.taint_tracking);
        assert!(agent_sec.pointer_mode);
        // filter_mode = false → explicitly disabled.
        assert!(agent_sec.filter_mode.is_none());
        // Only the two recognized degradation modes survive.
        assert_eq!(
            agent_sec.degradation,
            vec![crate::InputMode::Pointer, crate::InputMode::Filter]
        );

        let stage_a = bp.find_stage("a").unwrap();
        let a_sec = stage_a.security.as_ref().unwrap();
        assert_eq!(a_sec.filter_mode, Some(crate::FilterMode::Structured));

        // Stage c: filter_mode = true (a non-`false` bool) matches neither the
        // string arm nor the `Some(false)` arm, leaving filter_mode untouched.
        let stage_c = bp.find_stage("c").unwrap();
        let c_sec = stage_c.security.as_ref().unwrap();
        assert_eq!(
            c_sec.filter_mode,
            crate::SecurityConfig::default().filter_mode
        );
    }

    #[test]
    fn parse_manifest_security_empty_degradation_keeps_default() {
        // A degradation list with no recognized modes leaves `modes` empty, so
        // the `if !modes.is_empty()` assignment is skipped and the default is
        // retained.
        let toml = r#"
[agent]
name = "sec-empty-deg"

[security]
degradation = ["nonsense", "also-bad"]
"#;
        let bp = parse_manifest(toml).unwrap();
        let sec = bp.security.as_ref().unwrap();
        assert_eq!(
            sec.degradation,
            crate::SecurityConfig::default().degradation
        );
    }
}
