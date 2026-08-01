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
                // top level) or old fallbacks list - treat both as models entries.
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

                let request_timeout_secs =
                    mt.get("request_timeout_secs").and_then(|v| v.as_integer());
                let request_timeout_secs = request_timeout_secs
                    .filter(|&secs| secs >= 0)
                    .map(|secs| secs as u64);

                ModelConfig {
                    models,
                    allow_user_default,
                    parameters,
                    request_timeout_secs,
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
                                // "Revise - I'll describe changes" = "Call ask_user_text ..."
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
                                // abort_options = ["Abort - cancel this run"]
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
                                // edit_options = ["Add detail - expand a section"]
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
                     [stages.{stage_name}.model] and will be IGNORED - move the \
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

            // Parse per-stage batch_tool_hint override: opt an individual stage
            // in/out of the batch-tool-calls system-prompt hint (e.g. `false` for
            // a sequential validate stage). Absent ⇒ inherit agent/global.
            if let Some(bth) = stage_value.get("batch_tool_hint").and_then(|v| v.as_bool()) {
                stage.batch_tool_hint = Some(bth);
            }

            // Parse per-stage sandbox override: [stages.<name>.sandbox]
            if let Some(sandbox_table) = stage_value.get("sandbox").and_then(|v| v.as_table()) {
                stage.sandbox = Some(parse_sandbox_config(sandbox_table)?);
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

            // Parse per-stage context layout: [stages.<name>.context.regions].
            // Different stages can carry different region sets - the runtime swaps
            // to a stage's layout on entry (apply_stage_context → apply_layout),
            // preserving overlapping regions' content by name. Absent ⇒ the stage
            // inherits the global [context.regions] layout. NOTE (TOML nesting):
            // like [stages.<name>.model], this must be its own `[...]` section;
            // don't place `context = ...` inline keys after other sub-tables.
            if let Some(regions_table) = stage_value
                .get("context")
                .and_then(|v| v.get("regions"))
                .and_then(|v| v.as_table())
            {
                let (stage_regions, stage_total) = parse_region_layout(regions_table)?;
                stage.context_layout = Some(ContextLayout::new(stage_regions, stage_total));
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
                        Some("stuck") => TransitionCondition::Stuck,
                        Some("always") | None => TransitionCondition::Always,
                        // Reject unknown conditions rather than silently building a
                        // `Custom(..)` edge the runtime never evaluates (a dead edge).
                        Some(other) => {
                            return Err(Error::Other(format!(
                                "transition to '{target_name}' has unknown condition \
                                 '{other}' (valid: always, error, max_iterations, \
                                 llm_choice, stuck)"
                            )));
                        }
                    };

                    // Stuck thresholds live on the edge they arm, so a stage can
                    // be armed on iterations while another is armed on wall clock.
                    // Both halves are required together: a bare `condition =
                    // "stuck"` edge could never fire, and thresholds under any
                    // other condition would be silently ignored.
                    let stuck = parse_stuck_config(edge_value);
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
                    let gate = edge_value
                        .get("gate")
                        .and_then(|v| v.as_table())
                        .map(parse_transition_gate);

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

    // Parse agent-level batch_tool_hint override: `[agent] batch_tool_hint`.
    // Absent ⇒ inherit the global config toggle; a per-stage value overrides it.
    if let Some(bth) = agent.get("batch_tool_hint").and_then(|v| v.as_bool()) {
        blueprint.batch_tool_hint = Some(bth);
    }

    // Parse agent-level sandbox config: [sandbox]
    if let Some(sandbox_table) = parsed.get("sandbox").and_then(|v| v.as_table()) {
        blueprint.sandbox = Some(parse_sandbox_config(sandbox_table)?);
    }

    // Parse agent-level read-path declarations: [read_paths]. Entries are
    // syntax-checked here so a broken one fails `lev validate`/`lev add`/spawn
    // loudly, instead of degrading the agent at its first out-of-workdir read.
    if let Some(rp_table) = parsed.get("read_paths").and_then(|v| v.as_table()) {
        let mut allow = Vec::new();
        if let Some(entries) = rp_table.get("allow").and_then(|v| v.as_array()) {
            for entry in entries {
                let Some(raw) = entry.as_str() else {
                    return Err(Error::Other(format!(
                        "[read_paths] allow entries must be strings, got: {entry}"
                    )));
                };
                crate::read_paths::validate_entry_syntax(raw).map_err(Error::Other)?;
                allow.push(raw.to_string());
            }
        }
        blueprint.read_paths = Some(crate::blueprint::ReadPathsConfig { allow });
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

    // Parse repetition-detection config: [repetition_detection]
    if let Some(rd_table) = parsed
        .get("repetition_detection")
        .and_then(|v| v.as_table())
    {
        blueprint.repetition_detection = Some(crate::RepetitionDetectionConfig {
            max_repeat_calls: rd_table
                .get("max_repeat_calls")
                .and_then(|v| v.as_integer())
                .map(|v| v as usize),
            max_readonly_streak: rd_table
                .get("max_readonly_streak")
                .and_then(|v| v.as_integer())
                .map(|v| v as usize),
            enabled: rd_table.get("enabled").and_then(|v| v.as_bool()),
        });
    }

    // Parse cross-blueprint context transforms: [[transforms]]. Each maps a
    // parent (`from_blueprint`) region onto a child (`to_blueprint`) region when
    // a sub-agent is spawned, optionally transforming the content en route.
    if let Some(transforms_arr) = parsed.get("transforms").and_then(|v| v.as_array()) {
        for t in transforms_arr {
            let mappings = t
                .get("mappings")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().map(parse_region_mapping).collect())
                .unwrap_or_default();
            blueprint.transforms.push(ContextTransform {
                from_blueprint: str_field(t, "from_blueprint"),
                to_blueprint: str_field(t, "to_blueprint"),
                mappings,
            });
        }
    }

    Ok(blueprint)
}

/// A required-shaped string field, defaulting to empty when absent (the value's
/// meaning is validated later by `Blueprint::validate`).
fn str_field(v: &toml::Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Parse a transition edge's `stuck_after_*` thresholds into a [`StuckConfig`],
/// or `None` when the edge arms none of them.
///
/// Non-positive values read as unset - mirroring `enforce_max_iterations`, where
/// `max == 0` means "unlimited" - so `stuck_after_iterations = 0` leaves the edge
/// unarmed and the caller rejects it, rather than the edge firing on turn zero.
fn parse_stuck_config(edge: &toml::Value) -> Option<StuckConfig> {
    let threshold = |key: &str| {
        edge.get(key)
            .and_then(|v| v.as_integer())
            .filter(|v| *v > 0)
            .map(|v| v as usize)
    };
    let cfg = StuckConfig {
        after_iterations: threshold("stuck_after_iterations"),
        after_minutes: threshold("stuck_after_minutes"),
        after_same_file_edits: threshold("stuck_after_same_file_edits"),
        after_tool_calls: threshold("stuck_after_tool_calls"),
    };
    cfg.is_armed().then_some(cfg)
}

/// Parse one `[[transforms.mappings]]` entry. An omitted or unrecognized
/// `transform` yields `None` (a plain region copy at apply time).
fn parse_region_mapping(v: &toml::Value) -> RegionMapping {
    let transform = match v.get("transform").and_then(|x| x.as_str()) {
        Some("direct") => Some(ContentTransform::Direct),
        Some("summarize") => Some(ContentTransform::Summarize),
        Some("extract") => Some(ContentTransform::Extract {
            fields: v
                .get("fields")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        }),
        _ => None,
    };
    RegionMapping {
        from_region: str_field(v, "from_region"),
        to_region: str_field(v, "to_region"),
        transform,
    }
}

/// Parse a `[context.regions]` (or `[stages.<name>.context.regions]`) table into
/// region definitions plus the summed absolute-budget total.
///
/// Each region may express its ceiling as a percentage of the model context
/// window (`budget = "35%"`) with optional absolute guard-rails (`max_tokens`
/// caps it, `min_tokens` floors it), or as a plain absolute `max_tokens` (the
/// legacy form, default 5000). Compacting regions may set `compact_at = "80%"`
/// (compact at that fraction of the resolved budget) and/or an absolute
/// `threshold_tokens` cap. Percentage regions carry a provisional `max_tokens`
/// (the cap, or 0) that is finalized when the layout is resolved against a model
/// window at spawn - see [`ContextLayout::resolved`]. The returned total sums
/// only the absolute maxes; percentage regions contribute at resolution time.
///
/// Malformed `budget`/`compact_at` strings are a hard error so `leviath validate`
/// catches them at load.
fn parse_region_layout(
    regions_table: &toml::value::Table,
) -> Result<(Vec<RegionDefinition>, usize)> {
    let mut regions = Vec::new();
    let mut total_tokens = 0usize;

    for (region_name, region_value) in regions_table {
        // `budget = "N%"` opts a region into percentage mode; `max_tokens` then
        // becomes the absolute cap and `min_tokens` the absolute floor. Without a
        // `budget`, `max_tokens` is the literal ceiling (legacy behavior).
        let percent = match region_value.get("budget").and_then(|v| v.as_str()) {
            Some(s) => Some(crate::BudgetSpec::parse_budget(s).map_err(Error::Other)?),
            None => None,
        };
        let max_tokens_opt = region_value
            .get("max_tokens")
            .and_then(|v| v.as_integer())
            .map(|v| v as usize);
        let min_tokens = region_value
            .get("min_tokens")
            .and_then(|v| v.as_integer())
            .map(|v| v as usize);

        let budget = match percent {
            Some(percent) => crate::BudgetSpec::Percent {
                percent,
                min: min_tokens,
                max: max_tokens_opt,
            },
            None => crate::BudgetSpec::Absolute(max_tokens_opt.unwrap_or(5000)),
        };
        // Provisional resolved ceiling: the literal value for absolute regions,
        // the cap (or 0) for percentage regions until resolution overwrites it.
        let provisional_max_tokens = match &budget {
            crate::BudgetSpec::Absolute(n) => *n,
            crate::BudgetSpec::Percent { max, .. } => max.unwrap_or(0),
        };

        // Compacting regions carry a compaction trigger. Parse `compact_at` (a
        // fraction of the resolved budget) and the absolute `threshold_tokens`
        // guard, and reconcile them into (RegionDefinition.compact_at, the value
        // stored on RegionKind::Compacting) per the resolution contract in
        // `ContextLayout::resolve_compacting_threshold`.
        let compact_at = match region_value.get("compact_at").and_then(|v| v.as_str()) {
            Some(s) => Some(crate::BudgetSpec::parse_budget(s).map_err(Error::Other)?),
            None => None,
        };
        let explicit_threshold = region_value
            .get("threshold_tokens")
            .and_then(|v| v.as_integer())
            .map(|v| v as usize);

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
                let eviction_strategy = match region_value.get("strategy").and_then(|v| v.as_str())
                {
                    Some("bulk") => {
                        let overflow = region_value
                            .get("overflow")
                            .and_then(|v| v.as_integer())
                            .unwrap_or(10) as usize;
                        EvictionStrategy::Bulk { overflow }
                    }
                    Some("compact") => {
                        let compact_count = region_value
                            .get("compact_count")
                            .and_then(|v| v.as_integer())
                            .unwrap_or(10) as usize;
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
                // Reconcile compact_at / threshold_tokens into the value stored on
                // the kind (the absolute cap or the usize::MAX "no cap" sentinel);
                // resolution turns it into the concrete threshold.
                let threshold = match (compact_at, explicit_threshold, percent.is_some()) {
                    (Some(_), Some(cap), _) => cap,
                    (Some(_), None, _) => usize::MAX,
                    (None, Some(t), _) => t,
                    // No compact_at and no threshold: default to 80% of the budget
                    // for percentage regions (resolved later), else the legacy
                    // absolute `max_tokens * 8 / 10`.
                    (None, None, true) => usize::MAX,
                    (None, None, false) => provisional_max_tokens * 8 / 10,
                };
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
            "custom" => {
                let script = region_value
                    .get("script")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        Error::Other(format!(
                            "region '{region_name}': kind = \"custom\" requires \
                             script = \"<path>.rhai\""
                        ))
                    })?
                    .to_string();
                let persistent = region_value
                    .get("persistent")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                RegionKind::Custom { script, persistent }
            }
            unknown => {
                // A typo'd kind used to silently become Temporary - for a
                // custom region that would mean the script never runs, with
                // no signal anywhere. Fail at load instead; `lev validate`
                // surfaces this immediately.
                return Err(Error::Other(format!(
                    "region '{region_name}': unknown kind \"{unknown}\" (valid kinds: \
                     pinned, sliding_window, temporary, compacting, clearable, \
                     compact_history, hashmap, custom)"
                )));
            }
        };

        // The effective compact_at fraction to store on the region: an explicit
        // value, or the 80% default for a percentage-budget compacting region
        // with no explicit threshold (so it resolves relative to the budget).
        let compact_at_field = match (kind_str, compact_at, explicit_threshold, percent.is_some()) {
            ("compacting", Some(f), _, _) => Some(f),
            ("compacting", None, None, true) => Some(0.80),
            _ => None,
        };

        let required = region_value
            .get("required")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let required_message = region_value
            .get("required_message")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let seed = parse_region_seed(region_name, region_value.get("seed"));

        // Percentage regions contribute their (unknown) size at resolution, so
        // only absolute budgets add to the summed total here.
        if percent.is_none() {
            total_tokens += provisional_max_tokens;
        }

        let mut def = RegionDefinition::new(region_name.clone(), kind, provisional_max_tokens)
            .with_budget(budget)
            .with_required(required, required_message);
        if let Some(f) = compact_at_field {
            def = def.with_compact_at(f);
        }
        if let Some(seed) = seed {
            def = def.with_seed(seed);
        }
        regions.push(def);
    }

    Ok((regions, total_tokens))
}

/// Parse a region's `seed` value from `[context.regions.<name>]`.
///
/// String forms: `"task_input"` → caller input keyed `task` (the `--task`/prompt
/// text); any other string → caller input keyed by that string, with the
/// convenience alias `"input"` meaning "keyed by this region's own name".
/// Table forms: `{ glob = "…" }`, `{ files = [...] }`, `{ literal = "…" }`,
/// `{ rhai = "…" }`, `{ command = "…" }`, or `{ caller = "…" }`.
///
/// Back-compat: a region literally named `task` with no `seed` gets an implicit
/// `CallerInput { name: "task" }`, so unmodified blueprints seed the task text
/// exactly as before.
fn parse_region_seed(region_name: &str, value: Option<&toml::Value>) -> Option<RegionSeed> {
    let Some(value) = value else {
        return (region_name == "task").then(|| RegionSeed::CallerInput {
            name: "task".to_string(),
        });
    };
    match value {
        toml::Value::String(s) => Some(match s.as_str() {
            "task_input" => RegionSeed::CallerInput {
                name: "task".to_string(),
            },
            "input" => RegionSeed::CallerInput {
                name: region_name.to_string(),
            },
            other => RegionSeed::CallerInput {
                name: other.to_string(),
            },
        }),
        toml::Value::Table(t) => {
            if let Some(pattern) = t.get("glob").and_then(|v| v.as_str()) {
                Some(RegionSeed::Glob {
                    pattern: pattern.to_string(),
                })
            } else if let Some(files) = t.get("files").and_then(|v| v.as_array()) {
                Some(RegionSeed::Files {
                    paths: files
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect(),
                })
            } else if let Some(text) = t.get("literal").and_then(|v| v.as_str()) {
                Some(RegionSeed::Literal {
                    text: text.to_string(),
                })
            } else if let Some(script) = t.get("rhai").and_then(|v| v.as_str()) {
                Some(RegionSeed::Rhai {
                    script: script.to_string(),
                })
            } else if let Some(command) = t.get("command").and_then(|v| v.as_str()) {
                Some(RegionSeed::Command {
                    command: command.to_string(),
                })
            } else {
                t.get("caller")
                    .and_then(|v| v.as_str())
                    .map(|name| RegionSeed::CallerInput {
                        name: name.to_string(),
                    })
            }
        }
        _ => None,
    }
}

/// Parse a `[security]` / `[stages.X.security]` table into a `SecurityConfig`.
/// A present block defaults `taint_tracking` to `true` (block presence implies
/// intent to configure security); omit the block entirely to inherit the
/// broader (agent/global) setting.
/// Parse a transition edge's `gate = { ... }` table. Every key is optional; an
/// empty table yields a gate that blocks nothing (`require_modifications` off).
fn parse_transition_gate(table: &toml::value::Table) -> crate::blueprint::TransitionGate {
    let mut gate = crate::blueprint::TransitionGate::default();
    if let Some(rm) = table.get("require_modifications").and_then(|v| v.as_bool()) {
        gate.require_modifications = rm;
    }
    if let Some(msg) = table.get("message").and_then(|v| v.as_str()) {
        gate.message = Some(msg.trim().to_string());
    }
    if let Some(region) = table.get("region").and_then(|v| v.as_str()) {
        gate.region = Some(region.to_string());
    }
    if let Some(tools) = table.get("tools").and_then(|v| v.as_array()) {
        gate.tools = tools
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
    }
    // A negative budget is a typo, not "never hold the stage" - fall back to the
    // default rather than silently disabling the gate.
    if let Some(max) = table
        .get("max_attempts")
        .and_then(|v| v.as_integer())
        .filter(|max| *max >= 0)
    {
        gate.max_attempts = Some(max as usize);
    }
    gate
}

fn parse_security_config(security_table: &toml::value::Table) -> crate::SecurityConfig {
    let mut sc = crate::SecurityConfig::default();
    if let Some(tt) = security_table
        .get("taint_tracking")
        .and_then(|v| v.as_bool())
    {
        sc.taint_tracking = tt;
    }
    sc
}

/// Parse a `[sandbox]` / `[stages.X.sandbox]` table into a `ToolSandboxConfig`.
/// A present block with no `kind` means host passthrough; omit the block to
/// inherit the broader (agent/global) sandbox. An unknown `kind` or
/// `on_unavailable` value is a hard error rather than a silently-ignored
/// misconfiguration (mirrors transition-condition/transform validation).
fn parse_sandbox_config(table: &toml::value::Table) -> Result<crate::sandbox::ToolSandboxConfig> {
    use crate::sandbox::{OnUnavailable, SandboxKind, ToolSandboxConfig};

    let mut sc = ToolSandboxConfig::default();

    if let Some(kind) = table.get("kind").and_then(|v| v.as_str()) {
        sc.kind = match kind {
            "none" => SandboxKind::None,
            "namespace" => SandboxKind::Namespace,
            "container" => SandboxKind::Container,
            other => {
                return Err(Error::Other(format!(
                    "sandbox has unknown kind '{other}' \
                     (valid: none, namespace, container)"
                )));
            }
        };
    }
    if let Some(image) = table.get("image").and_then(|v| v.as_str()) {
        sc.image = Some(image.to_string());
    }
    if let Some(engine) = table.get("engine").and_then(|v| v.as_str()) {
        sc.engine = Some(engine.to_string());
    }
    if let Some(network) = table.get("network").and_then(|v| v.as_bool()) {
        sc.network = network;
    }
    if let Some(persist) = table.get("persist").and_then(|v| v.as_bool()) {
        sc.persist = persist;
    }
    if let Some(mounts) = table.get("mount").and_then(|v| v.as_array()) {
        sc.mounts = mounts
            .iter()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect();
    }
    if let Some(ou) = table.get("on_unavailable").and_then(|v| v.as_str()) {
        sc.on_unavailable = match ou {
            "error" => OnUnavailable::Error,
            "warn" => OnUnavailable::Warn,
            other => {
                return Err(Error::Other(format!(
                    "sandbox has unknown on_unavailable '{other}' \
                     (valid: error, warn)"
                )));
            }
        };
    }
    Ok(sc)
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
dynamic_tools = true

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
        assert!(bp.dynamic_tools);
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

[stages.implement]
mode = "autonomous"

[stages.error_handler]
mode = "autonomous"

[stages.timeout_handler]
mode = "autonomous"

[stages.choice_stage]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        let analyze = bp.find_stage("analyze").unwrap();
        assert_eq!(
            analyze.transition_prompt,
            Some("Pick the next stage".to_string())
        );
        let transitions = analyze.transitions.as_ref().unwrap();
        assert_eq!(transitions.len(), 4);

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
    }

    #[test]
    fn parse_manifest_rejects_unknown_transition_condition() {
        let toml = r#"
[agent]
name = "bad-cond"

[stages.analyze]
mode = "autonomous"

[stages.analyze.transitions.next]
condition = "whenever_i_feel_like_it"

[stages.next]
mode = "autonomous"
"#;
        let err = parse_manifest(toml).unwrap_err().to_string();
        assert!(err.contains("unknown condition"), "got: {err}");
        assert!(err.contains("whenever_i_feel_like_it"), "got: {err}");
        // The rejection message must list every condition that IS valid.
        assert!(err.contains("stuck"), "got: {err}");
    }

    // ─── stuck detection (#106) ─────────────────────────────────────────────

    /// Build a two-stage manifest whose `analyze → next` edge carries `body`.
    fn stuck_edge_manifest(body: &str) -> String {
        format!(
            r#"
[agent]
name = "stuck-agent"

[stages.analyze]
mode = "autonomous"

[stages.analyze.transitions.next]
{body}

[stages.next]
mode = "autonomous"
"#
        )
    }

    #[test]
    fn parse_manifest_reads_stuck_transition_thresholds() {
        let toml = stuck_edge_manifest(
            r#"condition = "stuck"
stuck_after_iterations = 20
stuck_after_minutes = 10
stuck_after_same_file_edits = 3
stuck_after_tool_calls = 60"#,
        );
        let bp = parse_manifest(&toml).unwrap();
        let edge = bp.stages[0]
            .transitions
            .as_ref()
            .unwrap()
            .get("next")
            .unwrap();
        assert_eq!(edge.condition, TransitionCondition::Stuck);
        let cfg = edge.stuck.expect("thresholds parsed");
        assert_eq!(cfg.after_iterations, Some(20));
        assert_eq!(cfg.after_minutes, Some(10));
        assert_eq!(cfg.after_same_file_edits, Some(3));
        assert_eq!(cfg.after_tool_calls, Some(60));
    }

    #[test]
    fn parse_manifest_reads_a_partially_armed_stuck_edge() {
        let toml = stuck_edge_manifest(
            r#"condition = "stuck"
stuck_after_same_file_edits = 5"#,
        );
        let bp = parse_manifest(&toml).unwrap();
        let cfg = bp.stages[0].transitions.as_ref().unwrap()["next"]
            .stuck
            .expect("thresholds parsed");
        assert_eq!(cfg.after_same_file_edits, Some(5));
        assert_eq!(cfg.after_iterations, None);
        assert_eq!(cfg.after_minutes, None);
        assert_eq!(cfg.after_tool_calls, None);
    }

    #[test]
    fn parse_manifest_rejects_a_stuck_condition_with_no_threshold() {
        let toml = stuck_edge_manifest(r#"condition = "stuck""#);
        let err = parse_manifest(&toml).unwrap_err().to_string();
        assert!(
            err.contains("condition 'stuck' but no threshold"),
            "got: {err}"
        );
    }

    /// A zero threshold reads as unset (mirroring `max_iterations = 0` meaning
    /// "unlimited"), so it leaves the edge dead rather than firing on turn zero.
    #[test]
    fn parse_manifest_treats_non_positive_stuck_thresholds_as_unset() {
        let toml = stuck_edge_manifest(
            r#"condition = "stuck"
stuck_after_iterations = 0
stuck_after_minutes = -5"#,
        );
        let err = parse_manifest(&toml).unwrap_err().to_string();
        assert!(
            err.contains("condition 'stuck' but no threshold"),
            "got: {err}"
        );
    }

    /// Thresholds under any other condition would silently never be read - the
    /// classic "I forgot `condition = \"stuck\"`" footgun.
    #[test]
    fn parse_manifest_rejects_stuck_thresholds_without_the_stuck_condition() {
        let toml = stuck_edge_manifest("stuck_after_iterations = 20");
        let err = parse_manifest(&toml).unwrap_err().to_string();
        assert!(err.contains("its condition is not 'stuck'"), "got: {err}");
    }

    #[test]
    fn parse_manifest_leaves_ordinary_edges_without_stuck_config() {
        let toml = stuck_edge_manifest(r#"hint = "go on""#);
        let bp = parse_manifest(&toml).unwrap();
        assert!(
            bp.stages[0].transitions.as_ref().unwrap()["next"]
                .stuck
                .is_none()
        );
    }

    #[test]
    fn parse_manifest_rejects_unknown_edge_transform() {
        let toml = r#"
[agent]
name = "bad-xform"

[stages.analyze]
mode = "autonomous"

[stages.analyze.transitions.next]
transform = "teleport"

[stages.next]
mode = "autonomous"
"#;
        let err = parse_manifest(toml).unwrap_err().to_string();
        assert!(err.contains("unknown transform"), "got: {err}");
        assert!(err.contains("teleport"), "got: {err}");
    }

    #[test]
    fn parse_manifest_custom_region_kind() {
        let toml = r#"
[agent]
name = "custom-region-test"

[context.regions]
brain = { kind = "custom", script = "context_hooks/brain.rhai", persistent = true, max_tokens = 5000 }
scratch = { kind = "custom", script = "context_hooks/scratch.rhai", max_tokens = 2000 }
"#;
        let bp = parse_manifest(toml).unwrap();
        let brain = bp
            .context_layout
            .regions
            .iter()
            .find(|r| r.name == "brain")
            .unwrap();
        assert_eq!(
            brain.kind,
            RegionKind::Custom {
                script: "context_hooks/brain.rhai".to_string(),
                persistent: true,
            }
        );
        // persistent defaults to false when omitted.
        let scratch = bp
            .context_layout
            .regions
            .iter()
            .find(|r| r.name == "scratch")
            .unwrap();
        assert_eq!(
            scratch.kind,
            RegionKind::Custom {
                script: "context_hooks/scratch.rhai".to_string(),
                persistent: false,
            }
        );
    }

    #[test]
    fn parse_manifest_custom_region_requires_script() {
        // Missing script is a load-time hard error, not a silent fallback.
        let missing = r#"
[agent]
name = "bad"

[context.regions]
brain = { kind = "custom", max_tokens = 5000 }
"#;
        let err = parse_manifest(missing).unwrap_err();
        assert!(
            err.to_string().contains("requires"),
            "actionable error: {err}"
        );

        // An empty/whitespace script path is equally dead - same error.
        let empty = r#"
[agent]
name = "bad"

[context.regions]
brain = { kind = "custom", script = "  ", max_tokens = 5000 }
"#;
        let err = parse_manifest(empty).unwrap_err();
        assert!(err.to_string().contains("requires"), "{err}");
    }

    #[test]
    fn parse_manifest_custom_region_with_percent_budget() {
        // Percentage budgets work on custom regions exactly as on built-ins.
        let toml = r#"
[agent]
name = "pct-custom"

[context.regions]
brain = { kind = "custom", script = "b.rhai", budget = "40%", min_tokens = 10000 }
"#;
        let bp = parse_manifest(toml).unwrap();
        let brain = bp
            .context_layout
            .regions
            .iter()
            .find(|r| r.name == "brain")
            .unwrap();
        assert!(brain.budget.is_percent());
        let resolved = bp.context_layout.resolved(200_000);
        assert_eq!(
            resolved
                .regions
                .iter()
                .find(|r| r.name == "brain")
                .unwrap()
                .max_tokens,
            80_000
        );
    }

    #[test]
    fn parse_manifest_per_stage_custom_region() {
        // A per-stage layout can declare a custom region only that stage uses.
        let toml = r#"
[agent]
name = "stage-custom"

[context.regions]
task = { kind = "pinned", max_tokens = 4000 }

[stages.plan]
[stages.plan.model]
provider = "anthropic"
model = "claude-sonnet-4"

[stages.plan.context.regions]
plan_view = { kind = "custom", script = "hooks/plan.rhai", max_tokens = 6000 }

[stages.implement]
[stages.implement.model]
provider = "anthropic"
model = "claude-sonnet-4"

[stages.plan.transitions.implement]
condition = "always"
"#;
        let bp = parse_manifest(toml).unwrap();
        let plan = bp.stages.iter().find(|s| s.name == "plan").unwrap();
        let layout = plan.context_layout.as_ref().unwrap();
        assert!(layout.regions.iter().any(|r| matches!(
            &r.kind,
            RegionKind::Custom { script, persistent: false } if script == "hooks/plan.rhai"
        )));
        // Sibling stage inherits the global layout (no per-stage override).
        let implement = bp.stages.iter().find(|s| s.name == "implement").unwrap();
        assert!(implement.context_layout.is_none());
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

        // Back-compat: with no `budget`, every region is an Absolute budget
        // matching its max_tokens, and compact_at stays None.
        assert_eq!(sys.budget, crate::BudgetSpec::Absolute(1000));
        assert_eq!(sys.compact_at, None);
        assert_eq!(comp.budget, crate::BudgetSpec::Absolute(6000));
        assert_eq!(comp.compact_at, None);
    }

    #[test]
    fn parse_region_percent_budget_with_guards() {
        let toml = r#"
[agent]
name = "pct"

[context.regions]
task = { kind = "pinned", budget = "2%", max_tokens = 4000, min_tokens = 500 }
free = { kind = "temporary", budget = "25%" }
abs  = { kind = "pinned", max_tokens = 3000 }
"#;
        let bp = parse_manifest(toml).unwrap();
        let task = bp.context_layout.get_region("task").unwrap();
        assert_eq!(
            task.budget,
            crate::BudgetSpec::Percent {
                percent: 0.02,
                min: Some(500),
                max: Some(4000),
            }
        );
        // Provisional max_tokens is the cap until resolution.
        assert_eq!(task.max_tokens, 4000);

        let free = bp.context_layout.get_region("free").unwrap();
        assert_eq!(
            free.budget,
            crate::BudgetSpec::Percent {
                percent: 0.25,
                min: None,
                max: None,
            }
        );
        // Percentage with no cap → provisional 0 until resolved.
        assert_eq!(free.max_tokens, 0);

        let abs = bp.context_layout.get_region("abs").unwrap();
        assert_eq!(abs.budget, crate::BudgetSpec::Absolute(3000));

        assert!(bp.context_layout.has_percent_budgets());
        // Only the absolute region contributes to the summed total.
        assert_eq!(bp.context_layout.total_budget_tokens, 3000);
    }

    #[test]
    fn parse_region_compact_at_variants() {
        let toml = r#"
[agent]
name = "compacting"

[context.regions]
capped   = { kind = "compacting", budget = "35%", compact_at = "80%", threshold_tokens = 32000 }
uncapped = { kind = "compacting", budget = "35%", compact_at = "80%" }
pct_only = { kind = "compacting", budget = "35%" }
absolute = { kind = "compacting", threshold_tokens = 9000 }
default  = { kind = "compacting", max_tokens = 10000 }
"#;
        let bp = parse_manifest(toml).unwrap();

        let capped = bp.context_layout.get_region("capped").unwrap();
        assert_eq!(capped.compact_at, Some(0.80));
        assert_eq!(
            capped.kind,
            RegionKind::Compacting {
                threshold_tokens: 32000
            }
        );

        let uncapped = bp.context_layout.get_region("uncapped").unwrap();
        assert_eq!(uncapped.compact_at, Some(0.80));
        assert_eq!(
            uncapped.kind,
            RegionKind::Compacting {
                threshold_tokens: usize::MAX
            }
        );

        // budget but no compact_at / threshold → 80% default (percentage mode).
        let pct_only = bp.context_layout.get_region("pct_only").unwrap();
        assert_eq!(pct_only.compact_at, Some(0.80));
        assert_eq!(
            pct_only.kind,
            RegionKind::Compacting {
                threshold_tokens: usize::MAX
            }
        );

        // Absolute threshold, no budget → absolute back-compat, compact_at None.
        let absolute = bp.context_layout.get_region("absolute").unwrap();
        assert_eq!(absolute.compact_at, None);
        assert_eq!(
            absolute.kind,
            RegionKind::Compacting {
                threshold_tokens: 9000
            }
        );

        // No budget, no compact_at, no threshold → legacy max_tokens * 8 / 10.
        let default = bp.context_layout.get_region("default").unwrap();
        assert_eq!(default.compact_at, None);
        assert_eq!(
            default.kind,
            RegionKind::Compacting {
                threshold_tokens: 8000
            }
        );
    }

    #[test]
    fn parse_region_rejects_malformed_budget() {
        let toml = r#"
[agent]
name = "bad"

[context.regions]
task = { kind = "pinned", budget = "lots" }
"#;
        let err = parse_manifest(toml).unwrap_err().to_string();
        assert!(err.contains("must end with '%'"), "{err}");
    }

    #[test]
    fn parse_region_rejects_out_of_range_budget() {
        let toml = r#"
[agent]
name = "bad"

[context.regions]
task = { kind = "pinned", budget = "150%" }
"#;
        let err = parse_manifest(toml).unwrap_err().to_string();
        assert!(err.contains("at most 100%"), "{err}");
    }

    #[test]
    fn parse_region_rejects_malformed_compact_at() {
        let toml = r#"
[agent]
name = "bad"

[context.regions]
work = { kind = "compacting", budget = "35%", compact_at = "eighty" }
"#;
        let err = parse_manifest(toml).unwrap_err().to_string();
        assert!(err.contains("must end with '%'"), "{err}");
    }

    #[test]
    fn parse_manifest_reads_per_stage_context_regions() {
        let toml = r#"
[agent]
name = "per-stage"
entry_stage = "plan"

[stages.plan]

[stages.plan.context.regions]
plan     = { kind = "pinned", budget = "20%", max_tokens = 40000 }
codebase = { kind = "compacting", budget = "30%", compact_at = "70%" }

[stages.implement]

[context.regions]
task = { kind = "pinned", max_tokens = 4000 }
"#;
        let bp = parse_manifest(toml).unwrap();

        // The plan stage has its own layout with percentage budgets.
        let plan_stage = bp.stages.iter().find(|s| s.name == "plan").unwrap();
        let plan_layout = plan_stage.context_layout.as_ref().unwrap();
        assert!(plan_layout.has_percent_budgets());
        let plan_region = plan_layout.get_region("plan").unwrap();
        assert_eq!(
            plan_region.budget,
            crate::BudgetSpec::Percent {
                percent: 0.20,
                min: None,
                max: Some(40000),
            }
        );
        let codebase = plan_layout.get_region("codebase").unwrap();
        assert_eq!(codebase.compact_at, Some(0.70));

        // The implement stage declared no regions → inherits the global layout.
        let implement_stage = bp.stages.iter().find(|s| s.name == "implement").unwrap();
        assert!(implement_stage.context_layout.is_none());
    }

    #[test]
    fn parse_manifest_rejects_malformed_per_stage_budget() {
        // A bad budget inside a [stages.X.context.regions] table propagates the
        // parse error just like the global layout does.
        let toml = r#"
[agent]
name = "bad-stage"
entry_stage = "plan"

[stages.plan]

[stages.plan.context.regions]
plan = { kind = "pinned", budget = "nope" }
"#;
        let err = parse_manifest(toml).unwrap_err().to_string();
        assert!(err.contains("must end with '%'"), "{err}");
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
    fn parse_manifest_reads_region_seed_shapes() {
        let toml = r#"
[agent]
name = "seed-test"

[stages.main]
mode = "autonomous"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[context.regions]
task = { kind = "pinned", max_tokens = 4000, seed = "task_input" }
criteria = { kind = "pinned", max_tokens = 2000, seed = "input" }
alias = { kind = "pinned", max_tokens = 2000, seed = "files_arg" }
specs = { kind = "pinned", max_tokens = 8000, seed = { glob = "specs/*.md" } }
config = { kind = "pinned", max_tokens = 2000, seed = { files = ["a.yaml", "b.yaml"] } }
lit = { kind = "pinned", max_tokens = 500, seed = { literal = "hello" } }
scripted = { kind = "pinned", max_tokens = 500, seed = { rhai = "init.rhai" } }
facts = { kind = "pinned", max_tokens = 500, seed = { command = "git ls-files" } }
plain = { kind = "pinned", max_tokens = 500 }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#;
        let bp = parse_manifest(toml).unwrap();
        let region = |n: &str| bp.context_layout.get_region(n).unwrap().seed.clone();
        assert_eq!(
            region("task"),
            Some(RegionSeed::CallerInput {
                name: "task".to_string()
            })
        );
        assert_eq!(
            region("criteria"),
            Some(RegionSeed::CallerInput {
                name: "criteria".to_string()
            }),
            "\"input\" is keyed by the region's own name"
        );
        assert_eq!(
            region("alias"),
            Some(RegionSeed::CallerInput {
                name: "files_arg".to_string()
            }),
            "a bare string is a caller-input key"
        );
        assert_eq!(
            region("specs"),
            Some(RegionSeed::Glob {
                pattern: "specs/*.md".to_string()
            })
        );
        assert_eq!(
            region("config"),
            Some(RegionSeed::Files {
                paths: vec!["a.yaml".to_string(), "b.yaml".to_string()]
            })
        );
        assert_eq!(
            region("lit"),
            Some(RegionSeed::Literal {
                text: "hello".to_string()
            })
        );
        assert_eq!(
            region("scripted"),
            Some(RegionSeed::Rhai {
                script: "init.rhai".to_string()
            })
        );
        assert_eq!(
            region("facts"),
            Some(RegionSeed::Command {
                command: "git ls-files".to_string()
            })
        );
        assert!(
            region("plain").is_none(),
            "a non-task region with no seed stays None"
        );
    }

    #[test]
    fn parse_manifest_region_seed_caller_table_and_non_seed_shapes() {
        let toml = r#"
[agent]
name = "seed-edges"

[stages.main]
mode = "autonomous"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[context.regions]
via_caller = { kind = "pinned", max_tokens = 500, seed = { caller = "extra" } }
unknown_tbl = { kind = "pinned", max_tokens = 500, seed = { nope = "x" } }
weird_type = { kind = "pinned", max_tokens = 500, seed = 42 }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#;
        let bp = parse_manifest(toml).unwrap();
        let region = |n: &str| bp.context_layout.get_region(n).unwrap().seed.clone();
        assert_eq!(
            region("via_caller"),
            Some(RegionSeed::CallerInput {
                name: "extra".to_string()
            })
        );
        // A table with none of the recognized keys → no seed.
        assert!(region("unknown_tbl").is_none());
        // A non-string, non-table seed value → no seed.
        assert!(region("weird_type").is_none());
    }

    #[test]
    fn parse_manifest_seeds_unnamed_task_region_by_default() {
        // A region literally named `task` with no explicit `seed` still gets an
        // implicit CallerInput seed, preserving pre-feature task seeding.
        let toml = r#"
[agent]
name = "implicit-task"

[stages.main]
mode = "autonomous"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[context.regions]
task = { kind = "pinned", max_tokens = 4000 }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#;
        let bp = parse_manifest(toml).unwrap();
        assert_eq!(
            bp.context_layout.get_region("task").unwrap().seed,
            Some(RegionSeed::CallerInput {
                name: "task".to_string()
            })
        );
    }

    #[test]
    fn parse_manifest_reads_repetition_detection() {
        let toml = r#"
[agent]
name = "rep-test"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[repetition_detection]
max_repeat_calls = 4
max_readonly_streak = 12
enabled = true
"#;
        let bp = parse_manifest(toml).unwrap();
        let rd = bp
            .repetition_detection
            .expect("repetition_detection parsed");
        assert_eq!(rd.max_repeat_calls, Some(4));
        assert_eq!(rd.max_readonly_streak, Some(12));
        assert_eq!(rd.enabled, Some(true));
    }

    #[test]
    fn parse_manifest_repetition_detection_absent_and_partial() {
        // Absent → None.
        let base = r#"
[agent]
name = "rep2"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"
"#;
        assert!(parse_manifest(base).unwrap().repetition_detection.is_none());
        // A partial block leaves the unset fields as None.
        let partial = format!("{base}\n[repetition_detection]\nenabled = false\n");
        let rd = parse_manifest(&partial)
            .unwrap()
            .repetition_detection
            .unwrap();
        assert_eq!(rd.enabled, Some(false));
        assert_eq!(rd.max_repeat_calls, None);
        assert_eq!(rd.max_readonly_streak, None);
    }

    #[test]
    fn parse_manifest_reads_transforms_with_all_mapping_kinds() {
        use crate::blueprint::ContentTransform;
        let toml = r#"
[agent]
name = "xform-test"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[[transforms]]
from_blueprint = "planner"
to_blueprint = "coder"

[[transforms.mappings]]
from_region = "plan"
to_region = "plan"
transform = "direct"

[[transforms.mappings]]
from_region = "notes"
to_region = "summary"
transform = "summarize"

[[transforms.mappings]]
from_region = "spec"
to_region = "fields"
transform = "extract"
fields = ["title", "owner"]

[[transforms.mappings]]
from_region = "misc"
to_region = "misc"
"#;
        // Render each parsed transform to a stable tag (ContentTransform has no
        // PartialEq); the four mappings exercise every arm.
        fn tag(t: &Option<ContentTransform>) -> String {
            match t {
                None => "none".to_string(),
                Some(ContentTransform::Direct) => "direct".to_string(),
                Some(ContentTransform::Summarize) => "summarize".to_string(),
                Some(ContentTransform::Extract { fields }) => {
                    format!("extract:{}", fields.join(","))
                }
            }
        }
        let bp = parse_manifest(toml).unwrap();
        assert_eq!(bp.transforms.len(), 1);
        let xf = &bp.transforms[0];
        assert_eq!(xf.from_blueprint, "planner");
        assert_eq!(xf.to_blueprint, "coder");
        assert_eq!(xf.mappings.len(), 4);
        assert_eq!(tag(&xf.mappings[0].transform), "direct");
        assert_eq!(tag(&xf.mappings[1].transform), "summarize");
        assert_eq!(tag(&xf.mappings[2].transform), "extract:title,owner");
        // A mapping with no `transform` key → None (plain copy at apply time).
        assert_eq!(tag(&xf.mappings[3].transform), "none");
        assert_eq!(xf.mappings[3].from_region, "misc");
    }

    #[test]
    fn parse_manifest_transform_unknown_kind_is_none() {
        let toml = r#"
[agent]
name = "xform-unknown"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"

[[transforms]]
from_blueprint = "a"
to_blueprint = "b"

[[transforms.mappings]]
from_region = "r"
to_region = "r"
transform = "bogus"
"#;
        let bp = parse_manifest(toml).unwrap();
        // An unrecognized transform is treated as a plain copy (None), never a panic.
        assert!(bp.transforms[0].mappings[0].transform.is_none());
    }

    #[test]
    fn parse_manifest_transforms_absent_is_empty() {
        let toml = r#"
[agent]
name = "no-xform"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"
"#;
        assert!(parse_manifest(toml).unwrap().transforms.is_empty());
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
    fn parse_manifest_read_paths_declarations() {
        let toml = r#"
[agent]
name = "rp-test"

[read_paths]
allow = ["~/.leviath/runs", "glob:~/design-docs/**", "regex:/data/archives/.*"]

[stages.plan]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        let rp = bp.read_paths.as_ref().unwrap();
        assert_eq!(
            rp.allow,
            vec![
                "~/.leviath/runs".to_string(),
                "glob:~/design-docs/**".to_string(),
                "regex:/data/archives/.*".to_string(),
            ]
        );
    }

    /// A `[read_paths]` section without an `allow` key parses as an empty
    /// declaration list - present but granting nothing to ask for.
    #[test]
    fn parse_manifest_read_paths_without_allow_is_empty() {
        let toml = r#"
[agent]
name = "rp-empty"

[read_paths]

[stages.plan]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        assert!(bp.read_paths.as_ref().unwrap().allow.is_empty());
    }

    /// No `[read_paths]` section means no declarations at all - the field
    /// stays `None` and the workdir sandbox is unchanged.
    #[test]
    fn parse_manifest_without_read_paths_leaves_none() {
        let toml = r#"
[agent]
name = "rp-none"

[stages.plan]
mode = "autonomous"
"#;
        assert!(parse_manifest(toml).unwrap().read_paths.is_none());
    }

    /// A malformed entry fails the whole parse - a skipped entry would
    /// degrade the agent silently at its first out-of-workdir read.
    #[test]
    fn parse_manifest_refuses_invalid_read_path_entries() {
        for (entry, expect) in [
            (r#""glob:[""#, "invalid glob"),
            (r#""regex:relative/.*""#, "must start with"),
            (r#"42"#, "must be strings"),
        ] {
            let toml = format!(
                r#"
[agent]
name = "rp-bad"

[read_paths]
allow = [{entry}]

[stages.plan]
mode = "autonomous"
"#
            );
            let err = parse_manifest(&toml).unwrap_err().to_string();
            assert!(err.contains(expect), "{entry}: {err}");
        }
    }

    #[test]
    fn parse_manifest_agent_and_stage_batch_tool_hint() {
        let toml = r#"
[agent]
name = "batch-test"
batch_tool_hint = true

[stages.plan]
mode = "autonomous"
batch_tool_hint = false

[stages.build]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        // Agent-level `[agent] batch_tool_hint` parsed.
        assert_eq!(bp.batch_tool_hint, Some(true));
        // Stage-level override opts this stage out.
        assert_eq!(bp.find_stage("plan").unwrap().batch_tool_hint, Some(false));
        // A stage with no override inherits (None).
        assert_eq!(bp.find_stage("build").unwrap().batch_tool_hint, None);
        // End-to-end cascade: plan resolves off, build inherits the agent's on.
        assert!(!crate::taint::resolve_batch_tool_hint(
            true,
            bp.batch_tool_hint,
            bp.find_stage("plan").unwrap().batch_tool_hint,
        ));
        assert!(crate::taint::resolve_batch_tool_hint(
            true,
            bp.batch_tool_hint,
            bp.find_stage("build").unwrap().batch_tool_hint,
        ));
    }

    #[test]
    fn parse_manifest_no_batch_tool_hint_is_none() {
        // No `batch_tool_hint` anywhere ⇒ both levels None ⇒ inherit global.
        let toml = r#"
[agent]
name = "no-batch"

[stages.main]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        assert_eq!(bp.batch_tool_hint, None);
        assert_eq!(bp.find_stage("main").unwrap().batch_tool_hint, None);
    }

    #[test]
    fn parse_manifest_security_block_without_taint_tracking_defaults_true() {
        // A present `[security]` block that omits `taint_tracking` keeps the
        // default (true) - block presence implies intent to configure security.
        let toml = r#"
[agent]
name = "sec-default"

[security]

[stages.main]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        assert!(bp.security.as_ref().unwrap().taint_tracking);
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
    fn parse_manifest_transition_gate_reads_every_key() {
        let toml = r#"
[agent]
name = "gate-test"

[stages.implement]
mode = "autonomous"
available_tools = ["write_file"]

[stages.implement.transitions.review]
hint = "done"
gate = { require_modifications = true, message = "  write something!  ", region = "implementation", tools = ["patch_file", 7], max_attempts = 2 }

[stages.review]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        let gate = bp
            .find_stage("implement")
            .unwrap()
            .transitions
            .as_ref()
            .unwrap()["review"]
            .gate
            .as_ref()
            .unwrap();
        assert!(gate.require_modifications);
        assert_eq!(gate.message.as_deref(), Some("write something!"));
        assert_eq!(gate.region.as_deref(), Some("implementation"));
        // Non-string entries in `tools` are skipped, not an error.
        assert_eq!(gate.tools, vec!["patch_file".to_string()]);
        assert_eq!(gate.max_attempts, Some(2));
    }

    #[test]
    fn parse_manifest_transition_gate_defaults_and_ignores_wrong_types() {
        let toml = r#"
[agent]
name = "gate-default-test"

[stages.a]
mode = "autonomous"

[stages.a.transitions.b]
hint = "no gate here"

[stages.a.transitions.c]
gate = { require_modifications = "yes", message = 3, region = [], tools = "write_file", max_attempts = -4 }

[stages.b]
mode = "autonomous"

[stages.c]
mode = "autonomous"
"#;
        let bp = parse_manifest(toml).unwrap();
        let transitions = bp.find_stage("a").unwrap().transitions.as_ref().unwrap();
        // An edge with no `gate` table has no gate at all.
        assert!(transitions["b"].gate.is_none());
        // A gate whose every key is the wrong type - including a negative
        // attempt budget - falls back to the defaults, i.e. a gate that blocks
        // nothing rather than one that silently never holds.
        let gate = transitions["c"].gate.as_ref().unwrap();
        assert_eq!(gate, &crate::blueprint::TransitionGate::default());
        // Zero, on the other hand, is a deliberate "record it but never hold".
        let toml = toml.replace("max_attempts = -4", "max_attempts = 0");
        let bp = parse_manifest(&toml).unwrap();
        assert_eq!(
            bp.find_stage("a").unwrap().transitions.as_ref().unwrap()["c"]
                .gate
                .as_ref()
                .unwrap()
                .max_attempts,
            Some(0)
        );
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
    fn parse_manifest_unknown_region_kind_is_a_hard_error() {
        // A typo'd kind used to silently become Temporary - for a custom
        // region that meant the script never ran, with no signal. The error
        // names the region, the bad value, and the valid kinds.
        let toml = r#"
[agent]
name = "unknown-kind"

[context.regions]
test = { kind = "cusotm", max_tokens = 1000 }
"#;
        let err = parse_manifest(toml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("region 'test'"), "{msg}");
        assert!(msg.contains("cusotm"), "{msg}");
        assert!(msg.contains("valid kinds"), "{msg}");
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

        // Default mode (no mode specified) - Autonomous
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

    // ─── Regression: shipped software-engineer agent must branch on plan_approval ──
    //
    // The "plan" stage's plan_approval interaction point lets the user pick
    // Approve / Revise / Add detail / Abort. If "plan" only has a single
    // outgoing transition edge, resolve_transition() auto-follows it without
    // ever consulting the LLM - so anything other than "Approve" is silently
    // ignored and the run proceeds to "implement" anyway. Guard against that
    // regressing by requiring at least two outgoing edges (forcing the
    // LLM-consultation path in resolve_transition / prompt_llm_transition).
    #[test]
    fn software_engineer_plan_stage_branches_on_choice() {
        let manifest_content =
            include_str!("../../leviath-cli/agents/software-engineer/agent.leviath");
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

    /// An implement stage that can leave without having written anything is
    /// how a run ends up with no output at all. Every edge out of
    /// `implement` that the AGENT can choose must carry a `require_modifications`
    /// gate, and the write tools must be routed into the region that gate points
    /// at (which is also what the reviewer is told to read).
    ///
    /// Runtime-fired edges (`error`, `stuck`) are exempt, and deliberately so: a
    /// gate answers "did you do any work before leaving?", which only makes sense
    /// for a voluntary exit. Gating an escape hatch would send a failed or
    /// looping agent back into the stage it is failing in - a stuck agent is not
    /// helped by being told to write more.
    #[test]
    fn shipped_coding_agents_gate_every_non_error_implement_edge() {
        for manifest_content in [
            include_str!("../../leviath-cli/agents/coder/agent.leviath"),
            include_str!("../../leviath-cli/agents/software-engineer/agent.leviath"),
        ] {
            let bp = parse_manifest(manifest_content).unwrap();
            bp.validate().unwrap();
            let implement = bp.find_stage("implement").unwrap();
            assert!(
                implement.available_tools.iter().any(|t| t == "write_file")
                    && implement.available_tools.iter().any(|t| t == "edit_file"),
                "{}: implement must advertise both write tools",
                bp.name
            );
            let transitions = implement.transitions.as_ref().unwrap();
            for (target, edge) in transitions {
                // Recovery must stay reachable from a failed stage, and a stuck
                // escape must stay reachable from a looping one.
                if matches!(
                    edge.condition,
                    crate::blueprint::TransitionCondition::Error
                        | crate::blueprint::TransitionCondition::Stuck
                ) {
                    continue;
                }
                assert!(
                    edge.gate.is_some(),
                    "{}: implement → {target} has no gate",
                    bp.name
                );
                let gate = edge.gate.as_ref().unwrap();
                assert!(gate.require_modifications, "{}: → {target}", bp.name);
                assert_eq!(
                    gate.region.as_deref(),
                    Some("implementation"),
                    "{}: → {target} must accept the persisted region as evidence",
                    bp.name
                );
            }
            // ...and the writes actually land in that region.
            let overrides = &implement
                .tool_result_routing
                .as_ref()
                .expect("implement must route tool results")
                .tool_overrides;
            for tool in ["write_file", "edit_file"] {
                assert_eq!(
                    overrides.get(tool).map(String::as_str),
                    Some("implementation"),
                    "{}: {tool} results must persist in `implementation`",
                    bp.name
                );
            }
        }
    }

    #[test]
    fn software_engineer_plan_routes_errors_and_cannot_end_the_run() {
        let manifest_content =
            include_str!("../../leviath-cli/agents/software-engineer/agent.leviath");
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

        // Planning must NOT be able to end the run. Abort is the engine's path
        // (abort_options on the plan_approval point), so `allow_complete` was
        // only ever a way for the model to stop early - and it did: one that had
        // created a file during `discover` read it back here, decided "already
        // created - no further action is needed", and completed without ever
        // reaching `implement`, taking the user's plan correction with it.
        assert!(!plan.allow_complete);
        // Every way out leads somewhere that does the work.
        for target in ["implement", "prototype"] {
            assert!(
                transitions.contains_key(target),
                "plan must be able to reach {target}"
            );
        }
    }

    #[test]
    fn software_engineer_plan_approval_option_routing() {
        let manifest_content =
            include_str!("../../leviath-cli/agents/software-engineer/agent.leviath");
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
        // "Approve" is a plain completing option - none of the above.
        assert!(!approval.directives.contains_key(&approve));
        assert!(!approval.abort_options.contains(&approve));
        assert!(!approval.edit_options.contains(&approve));
    }

    #[test]
    fn software_engineer_review_stage_can_complete_and_routes_errors() {
        let manifest_content =
            include_str!("../../leviath-cli/agents/software-engineer/agent.leviath");
        let bp = parse_manifest(manifest_content).unwrap();
        let review = bp.find_stage("review").unwrap();

        // review stage must allow_complete - an approving review has no real
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
        let manifest_content =
            include_str!("../../leviath-cli/agents/software-engineer/agent.leviath");
        let bp = parse_manifest(manifest_content).unwrap();
        bp.validate()
            .expect("shipped software-engineer blueprint must pass Blueprint::validate()");
    }

    #[test]
    fn software_engineer_plan_and_implement_can_ask_the_user_dynamically() {
        // Beyond the static plan_approval checkpoint, plan/implement should
        // be able to decide for themselves, mid-reasoning, that they need
        // human input - via the ask_user_* tools, not just the forced
        // interaction_points.
        let manifest_content =
            include_str!("../../leviath-cli/agents/software-engineer/agent.leviath");
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

    /// `interactive_points` mode with NO `interaction_points` array - the stage
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

    /// Per-stage tool_permissions with a non-string policy value - the inner
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

    /// Compaction config without `provider` - covers the None-branch at line ~460.
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
        // provider absent - stays at CompactionConfig default
        assert_eq!(cc.model, "gpt-4o-mini");
    }

    /// Compaction config without `model` - covers the None-branch at line ~463.
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
        // model absent - stays at CompactionConfig default
    }

    // ─── Security config parsing ──────────────────────────────────────────

    #[test]
    fn parse_manifest_with_security_config() {
        let toml = r#"
[agent]
name = "security-test"

[security]
taint_tracking = true
"#;
        let bp = parse_manifest(toml).unwrap();
        let sc = bp.security.as_ref().unwrap();
        assert!(sc.taint_tracking);
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
    fn parse_manifest_no_security_section() {
        let toml = r#"
[agent]
name = "no-security"
"#;
        let bp = parse_manifest(toml).unwrap();
        assert!(bp.security.is_none());
    }

    /// Agent-level tool_permissions with a non-string value - the inner
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
    fn parse_manifest_per_stage_request_timeout() {
        let toml = r#"
[agent]
name = "timeout-test"

[stages.analyze.model]
provider = "anthropic"
model = "claude-sonnet-5"
request_timeout_secs = 900

[stages.test_fix.model]
provider = "anthropic"
model = "claude-sonnet-5"
"#;
        let bp = parse_manifest(toml).unwrap();
        // Set on the stage that declares it.
        assert_eq!(
            bp.find_stage("analyze").unwrap().model.request_timeout_secs,
            Some(900)
        );
        // Absent → None (default job timeout applies).
        assert_eq!(
            bp.find_stage("test_fix")
                .unwrap()
                .model
                .request_timeout_secs,
            None
        );
    }

    #[test]
    fn parse_manifest_negative_request_timeout_is_ignored() {
        let toml = r#"
[agent]
name = "neg-timeout-test"

[stages.main.model]
provider = "anthropic"
model = "claude-sonnet-5"
request_timeout_secs = -5
"#;
        let bp = parse_manifest(toml).unwrap();
        // A nonsensical negative value is dropped rather than wrapping into a
        // huge u64.
        assert_eq!(
            bp.find_stage("main").unwrap().model.request_timeout_secs,
            None
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
    fn parse_manifest_security_agent_and_stage() {
        // Both an agent-level [security] and a stage-level override parse,
        // exercising both call sites of parse_security_config.
        let toml = r#"
[agent]
name = "sec-branches"

[security]
taint_tracking = false

[stages.a]
mode = "autonomous"

[stages.a.security]
taint_tracking = true
"#;
        let bp = parse_manifest(toml).unwrap();
        assert!(!bp.security.as_ref().unwrap().taint_tracking);
        let stage_a = bp.find_stage("a").unwrap();
        assert!(stage_a.security.as_ref().unwrap().taint_tracking);
    }

    #[test]
    fn parse_manifest_sandbox_agent_and_stage() {
        // Agent-level [sandbox] plus a tighter per-stage override, exercising
        // every field and both call sites of parse_sandbox_config.
        let toml = r#"
[agent]
name = "sandboxed"

[sandbox]
kind = "container"
image = "ubuntu:24.04"

[stages.implement]
mode = "autonomous"

[stages.implement.sandbox]
kind = "container"
image = "node:22-slim"
engine = "podman"
network = false
mount = ["/data", "/cache"]
persist = true
on_unavailable = "warn"
"#;
        let bp = parse_manifest(toml).unwrap();
        let agent = bp.sandbox.as_ref().unwrap();
        assert_eq!(agent.kind, crate::SandboxKind::Container);
        assert_eq!(agent.image.as_deref(), Some("ubuntu:24.04"));
        assert!(agent.network); // default when omitted
        assert_eq!(agent.engine, None); // auto-detect when omitted

        let stage = bp.find_stage("implement").unwrap();
        let sb = stage.sandbox.as_ref().unwrap();
        assert_eq!(sb.image.as_deref(), Some("node:22-slim"));
        assert_eq!(sb.engine.as_deref(), Some("podman"));
        assert!(!sb.network);
        assert_eq!(sb.mounts, vec!["/data".to_string(), "/cache".to_string()]);
        assert!(sb.persist);
        assert_eq!(sb.on_unavailable, crate::OnUnavailable::Warn);
    }

    #[test]
    fn parse_manifest_sandbox_kind_variants_and_no_kind() {
        // A `[sandbox]` block with no `kind` defaults to host (None); non-string
        // mount entries are filtered out.
        let bp = parse_manifest(
            "[agent]\nname = \"a\"\n\n\
             [sandbox]\nmount = [\"/ok\", 42]\n\n\
             [stages.s]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        let sb = bp.sandbox.unwrap();
        assert_eq!(sb.kind, crate::SandboxKind::None);
        assert_eq!(sb.mounts, vec!["/ok".to_string()]);

        // `namespace` and `none` kind strings both parse.
        let bp = parse_manifest(
            "[agent]\nname = \"a\"\n\n\
             [sandbox]\nkind = \"namespace\"\n\n\
             [stages.s]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        assert_eq!(bp.sandbox.unwrap().kind, crate::SandboxKind::Namespace);

        let bp = parse_manifest(
            "[agent]\nname = \"a\"\n\n\
             [sandbox]\nkind = \"none\"\n\n\
             [stages.s]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        assert_eq!(bp.sandbox.unwrap().kind, crate::SandboxKind::None);
    }

    #[test]
    fn parse_manifest_sandbox_explicit_error_policy_and_stage_error() {
        // Explicit `on_unavailable = "error"` exercises that match arm.
        let bp = parse_manifest(
            "[agent]\nname = \"a\"\n\n\
             [sandbox]\nkind = \"container\"\nimage = \"x\"\non_unavailable = \"error\"\n\n\
             [stages.s]\nmodel = { provider = \"anthropic\", model = \"m\" }\n",
        )
        .unwrap();
        assert_eq!(
            bp.sandbox.unwrap().on_unavailable,
            crate::OnUnavailable::Error
        );

        // An invalid *per-stage* sandbox propagates its error through the stage
        // parse (the `?` on the stage-level `parse_sandbox_config`).
        let err = parse_manifest(
            "[agent]\nname = \"a\"\n\n\
             [stages.s]\nmodel = { provider = \"anthropic\", model = \"m\" }\n\n\
             [stages.s.sandbox]\nkind = \"vm\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown kind 'vm'"), "got: {err}");
    }

    #[test]
    fn parse_manifest_sandbox_unknown_kind_errors() {
        let toml = r#"
[agent]
name = "bad"

[sandbox]
kind = "vm"
"#;
        let err = parse_manifest(toml).unwrap_err().to_string();
        assert!(err.contains("unknown kind 'vm'"), "got: {err}");
    }

    #[test]
    fn parse_manifest_sandbox_unknown_on_unavailable_errors() {
        let toml = r#"
[agent]
name = "bad"

[sandbox]
kind = "container"
on_unavailable = "explode"
"#;
        let err = parse_manifest(toml).unwrap_err().to_string();
        assert!(
            err.contains("unknown on_unavailable 'explode'"),
            "got: {err}"
        );
    }
}
