//! Parsing one `[stages.<name>]` table: its model, mode, tools, prompts,
//! transitions, and the per-stage knobs that hang off those.

use super::*;

/// Every key `parse_stage` reads off a `[stages.<name>]` table.
///
/// Kept beside the parser because it is only true of the parser: a key added
/// below and not added here is refused, which is the failure mode worth having
/// - the alternative was accepting it and doing nothing, forever.
pub(super) const STAGE_KEYS: &[&str] = &[
    "accepts_messages",
    "allow_as_worker",
    "allow_blocking_tools",
    "allow_complete",
    "available_connectors",
    "available_global_tools",
    "available_tools",
    "batch_tool_hint",
    "context",
    "description",
    "hooks",
    "interaction_points",
    "max_attempts",
    "max_items",
    "max_iterations",
    "max_revisits",
    "max_workers",
    "merge_stage",
    "mode",
    "model",
    "nudge",
    "on_worker_failure",
    "output",
    "require_output",
    "required_tools",
    "requires_children",
    "results_region",
    "sandbox",
    "security",
    "shell_hint",
    "split_prompt",
    "system_prompt",
    "tool_permissions",
    "tool_routing",
    "transition_prompt",
    "transitions",
    "worker_agent",
    "worker_query",
    "worker_stage",
];

/// Every key read off `[stages.<name>.context]`.
///
/// `regions` re-declares the whole layout for the stage; `hide` drops named
/// regions from an otherwise inherited one. Either way what is left out is
/// hidden rather than destroyed. An author who guesses any other key hears
/// about it instead of quietly carrying the region they meant to drop.
pub(super) const CONTEXT_KEYS: &[&str] = &["regions", "hide"];

/// The hooks this build implements, in the order the refusal names them.
/// `parse_stage_hooks` matches on each, and the schema guard in `tests.rs`
/// holds the published schema to the same list.
pub(super) const HOOK_KEYS: &[&str] = &[
    "on_stage_enter",
    "on_stage_exit",
    "before_inference",
    "after_inference",
    "on_tool_call",
    "on_completion",
    "on_error",
];

/// Every key read off `[stages.<name>.tool_routing]`.
pub(super) const TOOL_ROUTING_KEYS: &[&str] = &[
    "default_region",
    "max_result_tokens",
    "max_result_tokens_per_tool",
    "overrides",
    "persist",
];

/// Every key read off a transition edge's `gate = { ... }`.
pub(super) const GATE_KEYS: &[&str] = &[
    "max_attempts",
    "message",
    "region",
    "require_modifications",
    "require_no_open_items",
    "require_region_updated",
    "require_regions",
    "tools",
];

/// Every key read off one `[stages.<name>.transitions.<target>]` edge.
pub(super) const EDGE_KEYS: &[&str] = &[
    "condition",
    "gate",
    "hint",
    "stuck_after_iterations",
    "stuck_after_minutes",
    "stuck_after_same_file_edits",
    "stuck_after_tool_calls",
    "transform",
    "transform_config",
];

/// The three policies a `tool_permissions` entry may name.
pub(super) const TOOL_POLICIES: &[&str] = &["allow", "ask", "deny"];

/// Refuse a policy nothing will act on.
///
/// Resolution maps any unrecognised spelling to `ask`, so `shell = "denied"`
/// read as a prompt rather than a refusal - and `ask` is the *more* permissive
/// of the two, approvable by a session grant or `--yolo`. An author who
/// misspells a denial gets the opposite of what they wrote, in the layer where
/// that matters most.
///
/// The same typo in `config.toml` has always been refused, because that side
/// deserializes into an enum. This closes the gap the other way round.
pub(super) fn validate_tool_policy(where_: &str, tool: &str, policy: &str) -> Result<()> {
    if TOOL_POLICIES.contains(&policy.to_lowercase().as_str()) {
        return Ok(());
    }
    Err(Error::Other(format!(
        "{where_}: tool_permissions.{tool} = \"{policy}\" is not a policy \
         (valid: {})",
        TOOL_POLICIES.join(", ")
    )))
}

/// Refuse a key the parser does not read.
///
/// The manifest parser walks the TOML by hand, so an unrecognised key it does
/// not refuse is accepted and ignored: the blueprint is wrong in a way `lev
/// validate` calls good, and the only symptom is a stage behaving as though
/// the line had not been written. Region kinds and transition conditions are
/// strict for the same reason; this extends that to the tables holding them,
/// with the same "valid: …" phrasing.
pub(super) fn reject_unknown_keys(
    where_: &str,
    table: &toml::value::Table,
    allowed: &[&str],
) -> Result<()> {
    for key in table.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(Error::Other(format!(
                "{where_} has unknown key '{key}' (valid: {})",
                allowed.join(", ")
            )));
        }
    }
    Ok(())
}

/// Parse one entry of `[stages.<name>.tool_routing.overrides]`.
///
/// Two shapes, because the table answers two questions that authors reach for
/// together:
///
/// ```toml
/// read_file = "bulk"                                  # route it
/// read_file = { region = "bulk", max_result_tokens = 500 }   # route and cap it
/// ```
///
/// Anything else is an error rather than a skipped entry: skipping costs more
/// than the unsupported shape does, because the tool loses the region it named
/// *as well as* the cap and lands in `default_region` uncapped while `lev
/// validate` calls the blueprint good. A config that reads as if two limits
/// are in force while neither is costs real money before anyone thinks to
/// check.
fn parse_tool_override(
    stage_name: &str,
    tool_name: &str,
    value: &toml::Value,
    routing: &mut crate::blueprint::ToolResultRouting,
) -> Result<()> {
    let where_ = || format!("stage '{stage_name}': tool_routing.overrides.{tool_name}");

    if let Some(region_name) = value.as_str() {
        routing
            .tool_overrides
            .insert(tool_name.to_string(), region_name.to_string());
        return Ok(());
    }

    let Some(table) = value.as_table() else {
        return Err(Error::Other(format!(
            "{} must be a region name or a table of \
             {{ region, max_result_tokens }}, not {}",
            where_(),
            value.type_str()
        )));
    };

    for key in table.keys() {
        if !matches!(key.as_str(), "region" | "max_result_tokens") {
            return Err(Error::Other(format!(
                "{} has unknown key '{key}' (valid: region, max_result_tokens)",
                where_()
            )));
        }
    }

    if let Some(region) = table.get("region") {
        let region = region
            .as_str()
            .ok_or_else(|| Error::Other(format!("{}.region must be a region name", where_())))?;
        routing
            .tool_overrides
            .insert(tool_name.to_string(), region.to_string());
    }

    if let Some(max) = table.get("max_result_tokens") {
        let max = max.as_integer().ok_or_else(|| {
            Error::Other(format!("{}.max_result_tokens must be a number", where_()))
        })?;
        // `as usize` on a negative would wrap to an astronomically large cap,
        // which reads at a glance like "no limit" and is the opposite of what
        // was written.
        let max = usize::try_from(max).map_err(|_| {
            Error::Other(format!(
                "{}.max_result_tokens must not be negative (got {max})",
                where_()
            ))
        })?;
        routing
            .tool_max_result_tokens
            .insert(tool_name.to_string(), max);
    }

    if table.is_empty() {
        return Err(Error::Other(format!(
            "{} is empty (set region, max_result_tokens, or both)",
            where_()
        )));
    }

    Ok(())
}

/// Parse one `[stages.<name>]` table into a [`Stage`].
///
/// Reads nothing outside its own table, so the manifest's stage order is the
/// only thing the caller contributes.
pub(super) fn parse_stage(stage_name: &str, stage_value: &toml::Value) -> Result<Stage> {
    if let Some(table) = stage_value.as_table() {
        reject_unknown_keys(&format!("stage '{stage_name}'"), table, STAGE_KEYS)?;
    }

    let model_config = parse_stage_model(stage_name, stage_value)?;
    // A cap that does not parse fails the load. Left as a pass-through it
    // would reach the provider as a nonsense parameter or, worse, as no cap
    // at all, and a typo in a limit is the kind of mistake that only shows
    // up as a bill.
    if let Err(reason) = model_config.output_cap() {
        return Err(Error::Other(format!("stage '{stage_name}': {reason}")));
    }

    let mut stage = Stage::new(stage_name.to_string(), model_config);

    stage = apply_stage_mode(stage, stage_name, stage_value)?;

    let where_ = format!("stage '{stage_name}'");
    if let Some(max_iter) = count_of(stage_value, &where_, "max_iterations")? {
        stage.max_iterations = Some(max_iter);
    }

    if let Some(tools_arr) = array_of(stage_value, "available_tools") {
        stage.available_tools = tools_arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
    }

    // Whole MCP servers this stage may use, resolved at spawn against what
    // each actually advertises.
    if let Some(arr) = array_of(stage_value, "available_connectors") {
        stage.available_connectors = arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
    }

    // Human tools this stage keeps even when the run is unattended.
    // Validated against `available_tools` by `Stage::validate`.
    if let Some(tools_arr) = array_of(stage_value, "required_tools") {
        stage.required_tools = tools_arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
    }

    // Every bundled agent writes one and nothing read it: `Stage::description`
    // had a field and a builder, but the parser never looked at the key, so the
    // line was accepted and dropped. Found while enumerating the keys above,
    // and the same bug in miniature.
    if let Some(desc) = str_of(stage_value, "description") {
        stage.description = Some(desc.trim().to_string());
    }

    if let Some(sp) = str_of(stage_value, "system_prompt") {
        stage.config.insert(
            "system_prompt".to_string(),
            serde_json::Value::String(sp.trim().to_string()),
        );
    }

    // Warn on a common authoring mistake: a `system_prompt` written
    // *after* the `[stages.X.model]` sub-table lands under
    // `stages.X.model` (TOML nesting rules) and is silently ignored, so
    // the stage runs with no instructions. Point the author at the fix.
    let model_has_system_prompt = table_of(stage_value, "model")
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
    if let Some(routing_table) = table_of(stage_value, "tool_routing") {
        reject_unknown_keys(
            &format!("stage '{stage_name}': tool_routing"),
            routing_table,
            TOOL_ROUTING_KEYS,
        )?;
        let mut routing = crate::blueprint::ToolResultRouting::default();

        if let Some(dr) = str_of(routing_table, "default_region") {
            routing.default_region = dr.to_string();
        }
        if let Some(p) = bool_of(routing_table, "persist") {
            routing.persist = p;
        }
        if let Some(mt) = count_of(
            routing_table,
            &format!("{where_}: tool_routing"),
            "max_result_tokens",
        )? {
            routing.max_result_tokens = Some(mt);
        }
        if let Some(overrides_table) = table_of(routing_table, "overrides") {
            for (tool_name, region_val) in overrides_table {
                parse_tool_override(stage_name, tool_name, region_val, &mut routing)?;
            }
        }
        // Per-tool ceilings, spelled like the region overrides above so the two
        // tables read as the same idea applied to two different limits.
        if let Some(limits) = table_of(routing_table, "max_result_tokens_per_tool") {
            for (tool_name, max_val) in limits {
                let max = max_val.as_integer().ok_or_else(|| {
                    Error::Other(format!(
                        "stage '{stage_name}': \
                         tool_routing.max_result_tokens_per_tool.{tool_name} \
                         must be a number"
                    ))
                })?;
                let max = usize::try_from(max).map_err(|_| {
                    Error::Other(format!(
                        "stage '{stage_name}': \
                         tool_routing.max_result_tokens_per_tool.{tool_name} \
                         must not be negative (got {max})"
                    ))
                })?;
                routing
                    .tool_max_result_tokens
                    .insert(tool_name.clone(), max);
            }
        }

        stage.tool_result_routing = Some(routing);
    }

    // Parse requires_children flag
    if let Some(rc) = bool_of(stage_value, "requires_children") {
        stage.requires_children = rc;
    }

    // Parse allow_complete flag: lets the LLM end the run at this
    // stage (e.g. an approving review) instead of being forced down
    // its only/first transition edge.
    if let Some(ac) = bool_of(stage_value, "allow_complete") {
        stage.allow_complete = ac;
    }

    // Parse allow_as_worker flag: opts this stage in to being used as a
    // fan-out `worker_stage` target.
    if let Some(aw) = bool_of(stage_value, "allow_as_worker") {
        stage.allow_as_worker = aw;
    }

    // Whether the stage must hand back a final output. `mode = "output"`
    // means it by definition; any other stage opts in by hand (a fan-out
    // worker whose merge stage depends on its summary, say).
    if let Some(ro) = bool_of(stage_value, "require_output") {
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

    // `mode = "fan_out"` is sugar for granting the fan-out tool, on the same
    // reasoning as `mode = "output"` above: the grant lives on the Stage's own
    // tool list so `lev validate`, the tool filter and the lint all read the
    // same honest list, rather than the dispatch layer special-casing the mode.
    //
    // Added regardless of what the author wrote in `available_tools` - which for
    // a fan-out stage is usually `[]`, because the stage runs no tools of its
    // own. That empty list is a statement about the work, not about how the
    // stage starts its workers.
    if matches!(stage.mode, StageMode::FanOut { .. })
        && !stage
            .available_tools
            .iter()
            .any(|t| t == crate::blueprint::FAN_OUT_TOOL)
    {
        stage
            .available_tools
            .push(crate::blueprint::FAN_OUT_TOOL.to_string());
    }

    // Parse allow_blocking_tools flag: says this autonomous stage means
    // to offer `ask_user_*` / `present_for_review`, so `lev validate`
    // stops warning about it.
    if let Some(ab) = bool_of(stage_value, "allow_blocking_tools") {
        stage.allow_blocking_tools = ab;
    }

    // Parse available_global_tools flag: says this stage also advertises every
    // Rhai tool installed in the global `~/.leviath/tools/` directory, so a
    // tool an earlier run installed is offered without the blueprint naming it.
    if let Some(ag) = bool_of(stage_value, "available_global_tools") {
        stage.available_global_tools = ag;
    }

    // Parse per-stage security override: [stages.<name>.security]
    if let Some(sec_table) = table_of(stage_value, "security") {
        stage.security = Some(parse_security_config(sec_table));
    }

    // Parse per-stage batch_tool_hint override: opt an individual stage
    // in/out of the batch-tool-calls system-prompt hint (e.g. `false` for
    // a sequential validate stage). Absent ⇒ inherit agent/global.
    if let Some(bth) = bool_of(stage_value, "batch_tool_hint") {
        stage.batch_tool_hint = Some(bth);
    }

    // Parse per-stage shell_hint override: opt an individual stage
    // in/out of the platform shell hint. Absent ⇒ inherit agent/global.
    if let Some(sh) = bool_of(stage_value, "shell_hint") {
        stage.shell_hint = Some(sh);
    }

    // Parse per-stage nudge settings: [stages.<name>.nudge]. Absent ⇒
    // each field inherits agent/global.
    if let Some(nudge_table) = table_of(stage_value, "nudge") {
        stage.nudge = Some(parse_nudge_config(
            &format!("{where_}: nudge"),
            nudge_table,
        )?);
    }

    // Parse per-stage sandbox override: [stages.<name>.sandbox]
    if let Some(sandbox_table) = table_of(stage_value, "sandbox") {
        stage.sandbox = Some(parse_sandbox_config(&format!("{where_}: "), sandbox_table)?);
    }

    // Script-backed lifecycle hooks: [stages.<name>.hooks]
    if let Some(hooks_table) = table_of(stage_value, "hooks") {
        stage.hooks = parse_stage_hooks(stage_name, hooks_table)?;
    }

    // Parse the stage's declared output shape: [stages.<name>.output].
    // Narrows [agent.output]; whoever starts the run overrides both.
    if let Some(output_table) = table_of(stage_value, "output") {
        stage.output = Some(parse_output_spec(
            &format!("stage '{stage_name}': output"),
            output_table,
        )?);
    }

    // Parse accepts_messages flag: whether mid-run user messages are
    // injected into context between inference calls. Defaults to true
    // (via the Stage constructor); set false for stages that shouldn't
    // be interrupted (e.g. a final report generation stage).
    if let Some(am) = bool_of(stage_value, "accepts_messages") {
        stage.accepts_messages = am;
    }

    // Parse per-stage tool permissions: [stages.<name>.tool_permissions]
    if let Some(tp_table) = table_of(stage_value, "tool_permissions") {
        for (tool_name, policy_val) in tp_table {
            let policy_str = policy_val.as_str().ok_or_else(|| {
                Error::Other(format!(
                    "stage '{stage_name}': tool_permissions.{tool_name} must be \
                     one of {}",
                    TOOL_POLICIES.join(", ")
                ))
            })?;
            validate_tool_policy(&format!("stage '{stage_name}'"), tool_name, policy_str)?;
            stage
                .tool_permissions
                .insert(tool_name.clone(), policy_str.to_string());
        }
    }

    // Parse per-stage context layout: [stages.<name>.context.regions].
    // Different stages can carry different region sets - the runtime swaps
    // to a stage's layout on entry (apply_stage_context → apply_layout),
    // preserving overlapping regions' content by name. Absent ⇒ the stage
    // inherits the global [context.regions] layout. NOTE (TOML nesting):
    // like [stages.<name>.model], this must be its own `[...]` section;
    // don't place `context = ...` inline keys after other sub-tables.
    if let Some(context_table) = table_of(stage_value, "context") {
        reject_unknown_keys(
            &format!("stage '{stage_name}': context"),
            context_table,
            CONTEXT_KEYS,
        )?;
        if let Some(regions_table) = table_of(context_table, "regions") {
            let (stage_regions, stage_total) = parse_region_layout(regions_table)?;
            stage.context_layout = Some(ContextLayout::new(stage_regions, stage_total));
        }
        // `hide = ["sources"]`: the regions this stage leaves out of its
        // prompt. Names are checked against the blueprint once every layout is
        // known (`Blueprint::validate`); here only the shape is.
        if let Some(hide) = context_table.get("hide") {
            let names = hide
                .as_array()
                .and_then(|items| {
                    items
                        .iter()
                        .map(|v| v.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or_else(|| {
                    Error::Other(format!(
                        "stage '{stage_name}': context.hide must be a list of region names, \
                         e.g. hide = [\"sources\"]"
                    ))
                })?;
            stage.context_hide = names;
        }
    }

    // Parse max_revisits
    if let Some(mr) = count_of(stage_value, &where_, "max_revisits")? {
        stage.max_revisits = Some(mr);
    }

    // Parse transition_prompt
    if let Some(tp) = str_of(stage_value, "transition_prompt") {
        stage.transition_prompt = Some(tp.trim().to_string());
    }

    // Parse transitions: [stages.<name>.transitions.<target>]
    if let Some(transitions_table) = table_of(stage_value, "transitions") {
        stage.transitions = Some(parse_transitions(&where_, transitions_table)?);
    }

    Ok(stage)
}

/// Read a fan-out stage's `max_workers`, `max_items` or `max_attempts`.
///
/// `Ok(None)` when the key is absent, so the caller picks the default;
/// `Ok(Some(n))` for a non-negative integer. Anything else is an error rather
/// than a silent fallback: `max_workers = -1` would wrap to the largest
/// `usize` and so run unbounded, while `max_items = "twelve"` would read as no
/// cap at all - both of which show up as an unexpectedly wide fan-out, the
/// wrong place to first hear about a typo. `zero_means` is what `0` does, per key.
fn fan_out_number(
    stage_value: &toml::Value,
    stage_name: &str,
    key: &str,
    zero_means: &str,
) -> Result<Option<usize>> {
    let Some(value) = stage_value.get(key) else {
        return Ok(None);
    };
    let n = value.as_integer().ok_or_else(|| {
        Error::Other(format!(
            "stage '{stage_name}': {key} must be a whole number (0 means {zero_means})"
        ))
    })?;
    let n = usize::try_from(n).map_err(|_| {
        Error::Other(format!(
            "stage '{stage_name}': {key} must not be negative (got {n}; 0 means {zero_means})"
        ))
    })?;
    Ok(Some(n))
}

/// Apply `[stages.<name>] mode`, along with the sub-tables a given mode reads
/// (`interaction_points` for `interactive_points`, the fan-out block for
/// `fan_out`). A stage that names no mode keeps the constructor's default.
pub(super) fn apply_stage_mode(
    stage: Stage,
    stage_name: &str,
    stage_value: &toml::Value,
) -> Result<Stage> {
    let Some(mode_str) = str_of(stage_value, "mode") else {
        return Ok(stage);
    };
    Ok(match mode_str {
        "interactive" => stage.with_mode(StageMode::Interactive),
        "interactive_points" => {
            let mut points = Vec::new();
            if let Some(pts_arr) = array_of(stage_value, "interaction_points") {
                for pt in pts_arr {
                    let pt_name = str_of(pt, "name").unwrap_or("").to_string();
                    let pt_prompt = str_of(pt, "prompt").unwrap_or("").to_string();
                    let pt_required = bool_of(pt, "required").unwrap_or(true);
                    // What the point does when nobody is watching.
                    // Absent means auto-approve, the behaviour every
                    // `--yolo` run has had; `"ask"` opts a genuine
                    // human checkpoint out of that. A misspelling
                    // here would silently un-gate the checkpoint, so
                    // it is an error rather than a fallback.
                    let pt_unattended = match str_of(pt, "unattended") {
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
                    let pt_style = match str_of(pt, "style") {
                        Some("multiple_choice") => {
                            crate::blueprint::InteractionStyle::MultipleChoice
                        }
                        Some("confirm") => crate::blueprint::InteractionStyle::Confirm,
                        Some("free_text") | None => crate::blueprint::InteractionStyle::FreeText,
                        // Its neighbour `unattended` has always rejected an
                        // unknown value; this arm quietly turned a mistyped
                        // `confirm` into a free-text question with the options
                        // still listed and nothing enforcing them.
                        Some(other) => {
                            return Err(Error::Other(format!(
                                "stage '{stage_name}': interaction point style \
                                 \"{other}\" is not valid (valid: free_text, \
                                 multiple_choice, confirm)"
                            )));
                        }
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
                    let pt_abort_options: Vec<String> = array_of(pt, "abort_options")
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    // Options that open the last output for direct editing:
                    // edit_options = ["Add detail - expand a section"]
                    let pt_edit_options: Vec<String> = array_of(pt, "edit_options")
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    // Pinned region that holds the authoritative
                    // document: document_region = "plan"
                    let pt_document_region: Option<String> =
                        str_of(pt, "document_region").map(|s| s.to_string());
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
            let on_worker_failure = match str_of(stage_value, "on_worker_failure") {
                Some("fail_all") => crate::blueprint::WorkerFailurePolicy::FailAll,
                Some("continue") | None => crate::blueprint::WorkerFailurePolicy::Continue,
                // Refused rather than folded into continue: a misspelled
                // `fail_all` would let a fan-out swallow every worker failure -
                // the opposite of what was written, and invisible in a run that
                // then merged nothing.
                Some(other) => {
                    return Err(Error::Other(format!(
                        "stage '{stage_name}': on_worker_failure = \"{other}\" \
                         is not a policy (valid: continue, fail_all)"
                    )));
                }
            };
            let config = crate::blueprint::FanOutConfig {
                worker_agent: str_field("worker_agent"),
                worker_stage: str_field("worker_stage"),
                worker_query: str_field("worker_query"),
                merge_stage: str_field("merge_stage"),
                max_workers: fan_out_number(stage_value, stage_name, "max_workers", "unlimited")?
                    .unwrap_or(crate::blueprint::DEFAULT_MAX_WORKERS),
                on_worker_failure,
                split_prompt: str_field("split_prompt").unwrap_or_default(),
                results_region: str_field("results_region"),
                max_items: fan_out_number(stage_value, stage_name, "max_items", "unlimited")?
                    .filter(|n| *n > 0),
                max_attempts: fan_out_number(
                    stage_value,
                    stage_name,
                    "max_attempts",
                    "do not ask again",
                )?,
            };
            stage.with_mode(StageMode::FanOut { config })
        }
        "output" => stage.with_mode(StageMode::Output),
        "autonomous" => stage.with_mode(StageMode::Autonomous),
        // Refused rather than folded into autonomous: `mode =
        // "outupt"` would produce a stage that ran normally and
        // never asked for the output it was written to produce.
        // Region kinds reject an unknown `kind` for the same
        // reason. Any manifest this refuses is not doing what it
        // says.
        unknown => {
            return Err(Error::Other(format!(
                "stage '{stage_name}': unknown mode \"{unknown}\" (valid modes: \
                 autonomous, interactive, interactive_points, fan_out, output)"
            )));
        }
    })
}

/// Parse `[stages.<name>.hooks]` into the stage's [`StageHooks`].
///
/// An unknown key is a hard error rather than an ignored line. A blueprint that
/// writes `on_stage_entry` (or a hook this build does not implement yet) has
/// asked for behaviour it will silently not get, and a silently-ignored hook
/// reads exactly like one that ran and chose to do nothing.
pub(super) fn parse_stage_hooks(
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
                     (this build implements: {})",
                    HOOK_KEYS.join(", ")
                )));
            }
        }
    }
    Ok(hooks)
}

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
) -> Result<crate::blueprint::TransitionGate> {
    let mut gate = crate::blueprint::TransitionGate::default();
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
) -> Result<crate::blueprint::NudgeConfig> {
    let mut nudge = crate::blueprint::NudgeConfig::default();
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

/// Every key `parse_stage` reads off one `[[stages.<name>.interaction_points]]`
/// entry, for the schema guard in `tests.rs`. A list and not a check, like
/// `REGION_KEYS`: `options` and `choices` are the same setting under two
/// names, as are `directives` and `followups`.
#[cfg(test)]
pub(super) const INTERACTION_POINT_KEYS: &[&str] = &[
    "abort_options",
    "choices",
    "directives",
    "document_region",
    "edit_options",
    "followups",
    "name",
    "options",
    "prompt",
    "required",
    "style",
    "unattended",
];
