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
            .extend(tool_permission_metadata(tp_table));
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

/// Parse `[stages.<name>.model]`, or the shipped default when the stage does
/// not name one.
fn parse_stage_model(stage_value: &toml::Value) -> ModelConfig {
    let model_table = stage_value.get("model").and_then(|v| v.as_table());
    if let Some(mt) = model_table {
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

        let request_timeout_secs = mt.get("request_timeout_secs").and_then(|v| v.as_integer());
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
    }
}

/// Apply `[stages.<name>] mode`, along with the sub-tables a given mode reads
/// (`interaction_points` for `interactive_points`, the fan-out block for
/// `fan_out`). A stage that names no mode keeps the constructor's default.
fn apply_stage_mode(stage: Stage, stage_name: &str, stage_value: &toml::Value) -> Result<Stage> {
    let Some(mode_str) = stage_value.get("mode").and_then(|v| v.as_str()) else {
        return Ok(stage);
    };
    Ok(match mode_str {
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
                    let pt_required = pt.get("required").and_then(|v| v.as_bool()).unwrap_or(true);
                    // What the point does when nobody is watching.
                    // Absent means auto-approve, the behaviour every
                    // `--yolo` run has had; `"ask"` opts a genuine
                    // human checkpoint out of that. A misspelling
                    // here would silently un-gate the checkpoint, so
                    // it is an error rather than a fallback.
                    let pt_unattended = match pt.get("unattended").and_then(|v| v.as_str()) {
                        None | Some("auto_approve") => {
                            crate::blueprint::UnattendedPolicy::AutoApprove
                        }
                        Some("ask") => crate::blueprint::UnattendedPolicy::Ask,
                        Some(other) => {
                            return Err(Error::Other(format!(
                                "stage '{stage_name}': interaction point '{pt_name}' \
                                 has unattended = \"{other}\" - expected \"ask\" or \
                                 \"auto_approve\""
                            )));
                        }
                    };
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
                                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
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
                        unattended: pt_unattended,
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
                results_region: str_field("results_region"),
                max_items: stage_value
                    .get("max_items")
                    .and_then(|v| v.as_integer())
                    .filter(|n| *n > 0)
                    .map(|n| n as usize),
            };
            stage.with_mode(StageMode::FanOut { config })
        }
        "output" => stage.with_mode(StageMode::Output),
        "autonomous" => stage.with_mode(StageMode::Autonomous),
        // A misspelled mode used to become an autonomous stage in
        // silence, so `mode = "outupt"` produced a stage that ran
        // normally and never asked for the output it was written to
        // produce. Region kinds have always rejected an unknown
        // `kind` for the same reason; this brings stage modes into
        // line. Any manifest this refuses was already not doing what
        // it said.
        unknown => {
            return Err(Error::Other(format!(
                "stage '{stage_name}': unknown mode \"{unknown}\" (valid modes: \
                 autonomous, interactive, interactive_points, fan_out, output)"
            )));
        }
    })
}

/// Parse `[stages.<name>.transitions.<target>]` into the stage's edge map.
///
/// Unknown conditions and transforms are rejected here rather than degraded,
/// because both failure modes build an edge the runtime never takes and a
/// dead edge is invisible until the run wedges.
fn parse_transitions(
    transitions_table: &toml::value::Table,
) -> Result<std::collections::HashMap<String, TransitionEdge>> {
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
    Ok(transitions)
}

/// Parse one `[stages.<name>]` table into a [`Stage`].
///
/// Reads nothing outside its own table, so the manifest's stage order is the
/// only thing the caller contributes.
fn parse_stage(stage_name: &str, stage_value: &toml::Value) -> Result<Stage> {
    let model_config = parse_stage_model(stage_value);

    let mut stage = Stage::new(stage_name.to_string(), model_config);

    stage = apply_stage_mode(stage, stage_name, stage_value)?;

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

    // Human tools this stage keeps even when the run is unattended.
    // Validated against `available_tools` by `Stage::validate`.
    if let Some(tools_arr) = stage_value.get("required_tools").and_then(|v| v.as_array()) {
        stage.required_tools = tools_arr
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
    if let Some(routing_table) = stage_value.get("tool_routing").and_then(|v| v.as_table()) {
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
        if let Some(overrides_table) = routing_table.get("overrides").and_then(|v| v.as_table()) {
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

    // Whether the stage must hand back a final output. `mode = "output"`
    // means it by definition; any other stage opts in by hand (a fan-out
    // worker whose merge stage depends on its summary, say).
    if let Some(ro) = stage_value.get("require_output").and_then(|v| v.as_bool()) {
        stage.require_output = ro;
    }

    // `mode = "output"` is sugar for three settings, applied here rather
    // than in the mode arm above because `available_tools` and
    // `allow_complete` are read after it and would otherwise clobber
    // them. Writing them onto the Stage - instead of special-casing the
    // mode at dispatch - means `lev validate`, the tool filter, and the
    // lint all read one honest list.
    if stage.mode == StageMode::Output {
        stage.require_output = true;
        if !stage
            .available_tools
            .iter()
            .any(|t| t == crate::blueprint::SUBMIT_OUTPUT_TOOL)
        {
            stage
                .available_tools
                .push(crate::blueprint::SUBMIT_OUTPUT_TOOL.to_string());
        }
        // An output stage is normally the last thing a run does, so it
        // may end the run. An author who routes onward can say
        // `allow_complete = false` and be believed.
        if stage_value.get("allow_complete").is_none() {
            stage.allow_complete = true;
        }
    }

    // Parse allow_blocking_tools flag: says this autonomous stage means
    // to offer `ask_user_*` / `present_for_review`, so `lev validate`
    // stops warning about it.
    if let Some(ab) = stage_value
        .get("allow_blocking_tools")
        .and_then(|v| v.as_bool())
    {
        stage.allow_blocking_tools = ab;
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

    // Parse per-stage shell_hint override: opt an individual stage
    // in/out of the platform shell hint. Absent ⇒ inherit agent/global.
    if let Some(sh) = stage_value.get("shell_hint").and_then(|v| v.as_bool()) {
        stage.shell_hint = Some(sh);
    }

    // Parse per-stage nudge settings: [stages.<name>.nudge]. Absent ⇒
    // each field inherits agent/global.
    if let Some(nudge_table) = stage_value.get("nudge").and_then(|v| v.as_table()) {
        stage.nudge = Some(parse_nudge_config(nudge_table));
    }

    // Parse per-stage sandbox override: [stages.<name>.sandbox]
    if let Some(sandbox_table) = stage_value.get("sandbox").and_then(|v| v.as_table()) {
        stage.sandbox = Some(parse_sandbox_config(sandbox_table)?);
    }

    // Script-backed lifecycle hooks: [stages.<name>.hooks]
    if let Some(hooks_table) = stage_value.get("hooks").and_then(|v| v.as_table()) {
        stage.hooks = parse_stage_hooks(stage_name, hooks_table)?;
    }

    // Parse the stage's declared output shape: [stages.<name>.output].
    // Narrows [agent.output]; whoever starts the run overrides both.
    if let Some(output_table) = stage_value.get("output").and_then(|v| v.as_table()) {
        stage.output = Some(parse_output_spec(output_table));
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
    if let Some(transitions_table) = stage_value.get("transitions").and_then(|v| v.as_table()) {
        stage.transitions = Some(parse_transitions(transitions_table)?);
    }

    Ok(stage)
}

/// Parse `[stages.<name>.hooks]` into the stage's [`StageHooks`].
///
/// An unknown key is a hard error rather than an ignored line. A blueprint that
/// writes `on_stage_entry` (or a hook this build does not implement yet) has
/// asked for behaviour it will silently not get, and a silently-ignored hook
/// reads exactly like one that ran and chose to do nothing.
fn parse_stage_hooks(
    stage_name: &str,
    table: &toml::value::Table,
) -> Result<crate::blueprint::StageHooks> {
    let mut hooks = crate::blueprint::StageHooks::default();
    for (key, value) in table {
        let Some(path) = value.as_str() else {
            return Err(Error::Other(format!(
                "stage '{stage_name}': hook '{key}' must be a path to a .rhai file, got: {value}"
            )));
        };
        match key.as_str() {
            "on_stage_enter" => hooks.on_stage_enter = Some(path.to_string()),
            "on_stage_exit" => hooks.on_stage_exit = Some(path.to_string()),
            "before_inference" => hooks.before_inference = Some(path.to_string()),
            "after_inference" => hooks.after_inference = Some(path.to_string()),
            "on_tool_call" => hooks.on_tool_call = Some(path.to_string()),
            "on_completion" => hooks.on_completion = Some(path.to_string()),
            "on_error" => hooks.on_error = Some(path.to_string()),
            other => {
                return Err(Error::Other(format!(
                    "stage '{stage_name}': unknown hook '{other}' \
                     (this build implements: on_stage_enter, on_stage_exit, \
                     before_inference, after_inference, on_tool_call, \
                     on_completion, on_error)"
                )));
            }
        }
    }
    Ok(hooks)
}

/// Parse `[compaction]` over the defaults, leaving any field the manifest does
/// not mention at its default rather than at zero.
fn parse_compaction_config(table: &toml::value::Table) -> CompactionConfig {
    let mut cc = CompactionConfig::default();

    if let Some(provider) = table.get("provider").and_then(|v| v.as_str()) {
        cc.provider = provider.to_string();
    }
    if let Some(model) = table.get("model").and_then(|v| v.as_str()) {
        cc.model = model.to_string();
    }
    if let Some(sp) = table.get("system_prompt").and_then(|v| v.as_str()) {
        cc.system_prompt = Some(sp.to_string());
    }
    if let Some(mst) = table.get("max_summary_tokens").and_then(|v| v.as_integer()) {
        cc.max_summary_tokens = mst as usize;
    }
    if let Some(temp) = table.get("temperature").and_then(|v| v.as_float()) {
        cc.temperature = temp as f32;
    }

    cc
}

/// Parse `[read_paths]`. Entries are syntax-checked here so a broken one fails
/// `lev validate`/`lev add`/spawn loudly, instead of degrading the agent at its
/// first out-of-workdir read.
fn parse_read_paths(table: &toml::value::Table) -> Result<crate::blueprint::ReadPathsConfig> {
    let mut allow = Vec::new();
    if let Some(entries) = table.get("allow").and_then(|v| v.as_array()) {
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
    Ok(crate::blueprint::ReadPathsConfig { allow })
}

/// Parse `[safe_commands]`: what this agent would like to run unprompted.
///
/// Inert until the user opts in, so parsing is permissive - but a non-string
/// entry is still a hard error, because a list that silently loses members
/// reads as a grant that was made.
fn parse_safe_commands(table: &toml::value::Table) -> Result<crate::blueprint::SafeCommandsConfig> {
    let strings = |field: &str| -> Result<Vec<String>> {
        let Some(entries) = table.get(field).and_then(|v| v.as_array()) else {
            return Ok(Vec::new());
        };
        entries
            .iter()
            .map(|entry| {
                entry.as_str().map(str::to_string).ok_or_else(|| {
                    Error::Other(format!(
                        "[safe_commands] {field} entries must be strings, got: {entry}"
                    ))
                })
            })
            .collect()
    };
    Ok(crate::blueprint::SafeCommandsConfig {
        tools: strings("tools")?,
        shell: strings("shell")?,
    })
}

/// Flatten `[tool_permissions]` into the `tool_perm:<tool>` metadata keys the
/// permission layer reads. A non-string policy is dropped rather than rejected:
/// the value's meaning is validated where it is resolved.
fn tool_permission_metadata(
    table: &toml::value::Table,
) -> impl Iterator<Item = (String, serde_json::Value)> + use<'_> {
    table.iter().filter_map(|(tool_name, policy_val)| {
        policy_val.as_str().map(|policy| {
            (
                format!("tool_perm:{}", tool_name),
                serde_json::Value::String(policy.to_string()),
            )
        })
    })
}

/// Parse `[context.file_tracking]`. Tracking both directions into a `files`
/// region is the default because that is what the shipped layouts assume.
fn parse_file_tracking(table: &toml::value::Table) -> crate::FileTrackingConfig {
    crate::FileTrackingConfig {
        region: table
            .get("region")
            .and_then(|v| v.as_str())
            .unwrap_or("files")
            .to_string(),
        track_reads: table
            .get("track_reads")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        track_writes: table
            .get("track_writes")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        max_file_tokens: table
            .get("max_file_tokens")
            .and_then(|v| v.as_integer())
            .map(|v| v as usize),
    }
}

/// Parse `[repetition_detection]`. Every field stays `None` when absent so the
/// global config's value survives; there are no local defaults to apply here.
fn parse_repetition_detection(table: &toml::value::Table) -> crate::RepetitionDetectionConfig {
    crate::RepetitionDetectionConfig {
        max_repeat_calls: table
            .get("max_repeat_calls")
            .and_then(|v| v.as_integer())
            .map(|v| v as usize),
        max_readonly_streak: table
            .get("max_readonly_streak")
            .and_then(|v| v.as_integer())
            .map(|v| v as usize),
        enabled: table.get("enabled").and_then(|v| v.as_bool()),
    }
}

/// Parse one `[[transforms]]` entry: a parent region mapped onto a child region
/// when a sub-agent is spawned, optionally transformed en route.
fn parse_context_transform(t: &toml::Value) -> ContextTransform {
    ContextTransform {
        from_blueprint: str_field(t, "from_blueprint"),
        to_blueprint: str_field(t, "to_blueprint"),
        mappings: t
            .get("mappings")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().map(parse_region_mapping).collect())
            .unwrap_or_default(),
    }
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

/// Parse an `[agent.nudge]` / `[stages.X.nudge]` table into a `NudgeConfig`.
/// Every key is optional; an empty table is inert (each field still inherits
/// the broader level).
fn parse_nudge_config(table: &toml::value::Table) -> crate::blueprint::NudgeConfig {
    let mut nudge = crate::blueprint::NudgeConfig::default();
    if let Some(enabled) = table.get("enabled").and_then(|v| v.as_bool()) {
        nudge.enabled = Some(enabled);
    }
    // A negative count is a typo, not "never accept the text" - fall back to
    // inheriting rather than wrapping around.
    if let Some(max) = table
        .get("max")
        .and_then(|v| v.as_integer())
        .filter(|max| *max >= 0)
    {
        nudge.max = Some(max as usize);
    }
    if let Some(text) = table.get("text").and_then(|v| v.as_str()) {
        nudge.text = Some(text.trim().to_string());
    }
    nudge
}

/// Parse an `[agent.output]` or `[stages.<name>.output]` block.
///
/// `format` is read as an opaque string and never matched against a known set:
/// a value this parser has never seen is as valid as `"markdown"`, which is what
/// lets a blueprint ask for a2ui, a house schema, or a format invented after
/// this code was written without touching it.
///
/// `schema` is taken as arbitrary TOML and converted to JSON, so an author can
/// write the schema inline as a TOML table rather than embedding a JSON string.
fn parse_output_spec(table: &toml::value::Table) -> crate::output::OutputSpec {
    let string_field = |key: &str| {
        table
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
    };
    crate::output::OutputSpec {
        format: string_field("format"),
        instructions: string_field("instructions"),
        example: string_field("example"),
        // A schema that will not convert is dropped rather than fatal: the
        // validator itself already treats an uncompilable schema as "skip the
        // check" rather than "refuse every submission", and disagreeing here
        // would make the same bad schema fatal at load and harmless at dispatch.
        schema: table
            .get("schema")
            .and_then(|v| serde_json::to_value(v).ok()),
        validator: string_field("validator"),
    }
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
mod tests;
