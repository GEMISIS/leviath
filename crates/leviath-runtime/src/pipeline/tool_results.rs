//! Applying completed tool batches: results, file tracking, modification accounting.

use super::*;

/// The receiving end of the tool-outcomes channel, as a world resource.
#[derive(Resource)]
pub struct ToolResults(pub UnboundedReceiver<ToolOutcome>);

/// Apply a completed tool batch to an agent's context window: add the assistant
/// turn (with its tool calls) then each tool result, honoring the stage's
/// tool-result routing (target region, `persist=false`→scratch, per-result
/// truncation) and, when a per-tool sensitivity is provided, tagging the result
/// with that taint level. Tool results MUST be added (Anthropic requires a
/// `tool_result` for every `tool_use`), so an over-budget region truncates or
/// falls back to a placeholder rather than dropping. Ported from the core of
/// `AgentEngine::loop_apply_tool_results` (repetition + message draining are
/// separate systems).
pub(crate) fn apply_tool_results(
    window: &mut ContextWindow,
    response_content: &str,
    tool_calls: &[crate::components::ToolCall],
    tool_results: &[(String, String)],
    routing: Option<&leviath_core::blueprint::ToolResultRouting>,
    sensitivities: Option<&std::collections::HashMap<String, leviath_core::TaintLevel>>,
) {
    let response_tokens = leviath_core::estimate_tokens(response_content);
    let serialized: Vec<leviath_core::SerializedToolCall> = tool_calls
        .iter()
        .map(|tc| leviath_core::SerializedToolCall {
            id: tc.tool_id.clone(),
            name: tc.name.clone(),
            arguments: tc.arguments.clone(),
            thought_signature: tc.thought_signature.clone(),
        })
        .collect();
    let _ = window.add_typed_entry(
        "conversation",
        leviath_core::EntryKind::AssistantTurn {
            tool_calls: serialized,
        },
        response_content.to_string(),
        response_tokens,
    );

    for (tool_call_id, result) in tool_results {
        let mut result_text = result.clone();
        let tool_name = tool_calls
            .iter()
            .find(|tc| tc.tool_id == *tool_call_id)
            .map(|tc| tc.name.clone())
            .unwrap_or_default();

        // The tool's own ceiling when it has one, else the stage's.
        let tool_cap = routing.and_then(|r| {
            // Both sides canonicalized, exactly as `tool_overrides` below: the
            // author writes `bash`, the model calls `shell`, and a literal
            // comparison would silently miss in either direction.
            let canon = leviath_tools::canonical_tool_name(&tool_name);
            r.tool_max_result_tokens
                .iter()
                .find(|(k, _)| leviath_tools::canonical_tool_name(k) == canon)
                .map(|(_, v)| *v)
                .or(r.max_result_tokens)
        });
        if let Some(max_tokens) = tool_cap {
            let max_chars = max_tokens * 4;
            if result_text.len() > max_chars {
                result_text = truncate_on_char_boundary(&result_text, max_chars);
                result_text.push_str("\n[...truncated]");
            }
        }
        let result_tokens = leviath_core::estimate_tokens(&result_text);

        let base_region = match routing {
            Some(r) => {
                // Match overrides by CANONICAL tool name so a `bash = "..."` override
                // routes the `shell` tool (bash is an alias - the model calls the
                // canonical `shell`, so a literal-key lookup would silently miss).
                let canon = leviath_tools::canonical_tool_name(&tool_name);
                r.tool_overrides
                    .iter()
                    .find(|(k, _)| leviath_tools::canonical_tool_name(k) == canon)
                    .map(|(_, v)| v.as_str())
                    .unwrap_or(r.default_region.as_str())
            }
            None => "conversation",
        };
        let target_region = match routing {
            Some(r) if !r.persist && window.get_region("scratch").is_some() => "scratch",
            _ => base_region,
        };

        let taint_level = sensitivities.map(|s| {
            s.get(&tool_name)
                .copied()
                .unwrap_or(leviath_core::TaintLevel::Public)
        });
        // Add `content` (with entry `kind`) to `region`, honoring taint and falling
        // back to a truncated (then omitted) entry if the region is full.
        let add_kind = |window: &mut ContextWindow,
                        region: &str,
                        kind: leviath_core::EntryKind,
                        content: String,
                        tokens: usize| {
            let put = |w: &mut ContextWindow, c: String, t: usize| match taint_level {
                Some(level) => w.add_typed_tainted_to_region(region, kind.clone(), c, t, level),
                None => w.add_typed_entry(region, kind.clone(), c, t),
            };
            if put(window, content.clone(), tokens).is_err() {
                let available = window
                    .get_region(region)
                    .map(|r| r.max_tokens.saturating_sub(r.current_tokens))
                    .unwrap_or(0);
                let truncated = if available > 100 {
                    let char_budget = (available - 10) * 4;
                    let prefix = truncate_on_char_boundary(&content, char_budget);
                    let omitted = content.len().saturating_sub(prefix.len());
                    format!("{}... [truncated, {} chars omitted]", prefix, omitted)
                } else {
                    "[tool result truncated - context window full]".to_string()
                };
                let trunc_tokens = leviath_core::estimate_tokens(&truncated);
                if put(window, truncated, trunc_tokens).is_err() {
                    let _ = put(window, "[result omitted]".to_string(), 5);
                }
            }
        };
        let result_kind = || leviath_core::EntryKind::ToolResult {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            is_error: false,
        };

        if target_region == "conversation" {
            // Not routed (or routed back to the message stream): the tool_result
            // lives in `conversation`, paired with its tool_use.
            add_kind(
                window,
                "conversation",
                result_kind(),
                result_text,
                result_tokens,
            );
        } else {
            // Routed to a knowledge region. Anthropic requires each tool_result to sit
            // in the message immediately after its tool_use, so the PAIR must stay in
            // `conversation`: we keep a short pointer tool_result there (valid + cheap)
            // and store the FULL output in the target region as TEXT. Text renders as a
            // stable knowledge block for any region kind - a ToolResult block in a
            // second sliding_window would desync from its tool_use (→ API 400), and
            // dropping the conversation tool_result would orphan the tool_use (the
            // assembler strips it, so the model can't see its own call landed → loops).
            let preview: String = result_text.chars().take(160).collect();
            let ellipsis = if result_text.len() > preview.len() {
                "…"
            } else {
                ""
            };
            // A region this stage does not render is one the model cannot go
            // and read, so telling it to is worse than saying nothing: the
            // instruction is unfollowable and the model spends turns trying
            // (#370). `lev validate` refuses a blueprint that routes this way,
            // so reaching here means a layout swapped underneath a routing rule
            // rather than an author mistake - but the model still needs to be
            // told the truth about where its output went.
            let pointer = match window.hidden.contains(target_region) {
                false => format!(
                    "[output stored in context region '{target_region}' ({result_tokens} tokens) - read that region for the full result. Preview: {preview}{ellipsis}]"
                ),
                true => format!(
                    "[output stored in context region '{target_region}' ({result_tokens} tokens), which this stage does not carry - it is kept for a later stage and cannot be read from here. Preview: {preview}{ellipsis}]"
                ),
            };
            let pointer_tokens = leviath_core::estimate_tokens(&pointer);
            add_kind(
                window,
                "conversation",
                result_kind(),
                pointer,
                pointer_tokens,
            );
            add_kind(
                window,
                target_region,
                leviath_core::EntryKind::Text,
                result_text,
                result_tokens,
            );
        }
    }
}

/// Truncate a file body to `max_tokens` (≈4 chars/token) with a marker, or return
/// it unchanged when no cap is set or it already fits.
pub(crate) fn truncate_file(content: String, max_tokens: Option<usize>) -> String {
    match max_tokens {
        Some(max) => {
            let approx_chars = max * 4;
            if content.len() > approx_chars {
                let head: String = content.chars().take(approx_chars).collect();
                format!("{head}\n\n[... truncated at {max} tokens ...]")
            } else {
                content
            }
        }
        None => content,
    }
}

/// File tracking: for each `read_file`/`write_file` result (per the stage's
/// [`FileTrackingConfig`](leviath_core::blueprint::FileTrackingConfig)), upsert
/// the file body into the configured HashMap region (keyed by path, so re-reads
/// de-dup) and replace the inline tool result with a short reference - keeping
/// large file bodies out of the rolling conversation. No-op unless the region
/// exists and is a HashMap. `read_file`'s body is the result; `write_file`'s is
/// its `content` argument (no re-read needed in the ECS).
pub(crate) fn apply_file_tracking(
    window: &mut ContextWindow,
    ft: &leviath_core::blueprint::FileTrackingConfig,
    tool_calls: &[crate::components::ToolCall],
    merged: &mut [(String, String)],
) {
    let is_hashmap = window
        .get_region(&ft.region)
        .is_some_and(|r| matches!(r.kind, leviath_core::RegionKind::HashMap { .. }));
    if !is_hashmap {
        return;
    }
    for (call, (_id, result)) in tool_calls.iter().zip(merged.iter_mut()) {
        if call_had_no_effect(result) {
            continue;
        }
        let Some(path) = call.arguments.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        let (body, verb) = match call.name.as_str() {
            "read_file" if ft.track_reads => (result.clone(), "stored"),
            "write_file" if ft.track_writes => {
                match call.arguments.get("content").and_then(|v| v.as_str()) {
                    Some(c) => (c.to_string(), "written"),
                    None => continue,
                }
            }
            _ => continue,
        };
        let body = truncate_file(body, ft.max_file_tokens);
        let tokens = leviath_core::estimate_tokens(&body);
        window
            .get_region_mut(&ft.region)
            .expect("region presence checked above")
            .upsert_by_key(path, body, tokens)
            .ok();
        *result = format!(
            "File {verb} in [{}] → ### [{}] ({} tokens). Reference it there; do not re-read this path.",
            ft.region, path, tokens
        );
    }
}

/// The tool names that count as a file modification for the agent's current
/// stage: the built-in [`MODIFYING_TOOLS`](leviath_core::blueprint::MODIFYING_TOOLS)
/// plus any extra names declared by that stage's outgoing transition gates (for
/// agents whose writes go through MCP or script tools). All canonical, so a
/// `bash`-style alias in a gate's `tools` list still matches its real tool.
pub(crate) fn stage_modifying_tools(
    blueprint: Option<&AgentBlueprint>,
    cursor: Option<&StageCursor>,
) -> Vec<String> {
    let mut names: Vec<String> = leviath_core::blueprint::MODIFYING_TOOLS
        .iter()
        .map(|t| (*t).to_string())
        .collect();
    let (Some(bp), Some(cursor)) = (blueprint, cursor) else {
        return names;
    };
    let Some(stage) = bp.0.stages.get(cursor.index) else {
        return names;
    };
    let Some(transitions) = &stage.transitions else {
        return names;
    };
    for edge in transitions.values() {
        let Some(gate) = &edge.gate else { continue };
        for tool in &gate.tools {
            let canonical = leviath_tools::canonical_tool_name(tool).to_string();
            if !names.contains(&canonical) {
                names.push(canonical);
            }
        }
    }
    names
}

/// Tally this batch's file-modifying tool calls onto the stage's progress and the
/// run's outcome flags. A result prefixed `[denied]` (permission layer) counts as
/// *blocked* rather than successful - the agent tried and was refused, which a
/// gate treats differently from never having tried. `[error]` results (the write
/// itself failed) count as neither.
pub(crate) fn record_modifications(
    tool_calls: &[crate::components::ToolCall],
    merged: &[(String, String)],
    modifying: &[String],
    progress: Option<bevy_ecs::prelude::Mut<'_, StageProgress>>,
    flags: Option<bevy_ecs::prelude::Mut<'_, crate::persistence::RunOutcomeFlags>>,
) {
    let mut progress = progress;
    let mut flags = flags;
    for (call, (_id, result)) in tool_calls.iter().zip(merged.iter()) {
        let canonical = leviath_tools::canonical_tool_name(&call.name);
        if !modifying.iter().any(|t| t == canonical) {
            continue;
        }
        if result.starts_with("[denied]") {
            if let Some(progress) = progress.as_mut() {
                progress.blocked_modification_calls += 1;
            }
            continue;
        }
        if call_had_no_effect(result) {
            continue;
        }
        if let Some(progress) = progress.as_mut() {
            progress.modifying_tool_calls += 1;
        }
        if let Some(flags) = flags.as_mut() {
            let path = call
                .arguments
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>");
            flags.0.record_modification(path);
        }
    }
}

/// What `collect_tools` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type ToolQuery = (
    &'static mut ContextWindow,
    &'static crate::components::InferenceResult,
    Option<&'static crate::components::ToolResultRoutingComponent>,
    Option<&'static ToolSensitivities>,
    Option<&'static ContextToolResults>,
    Option<&'static StageCursor>,
    Option<&'static mut StageIoBuffer>,
    Option<&'static AgentBlueprint>,
    Option<&'static mut crate::repetition::RepetitionDetector>,
    Option<&'static mut StageProgress>,
    Option<&'static mut crate::persistence::RunOutcomeFlags>,
    Option<&'static mut crate::telemetry::StageActivity>,
    (
        Option<&'static crate::persistence::RunMetadata>,
        Option<&'static crate::components::AgentState>,
    ),
);

/// Tool-collect system: drain finished tool batches and apply them. Results are
/// written into the agent's context window (routing/truncation/taint honored)
/// and the agent loops back to `ReadyToInfer`. Outcomes for agents no longer
/// `AwaitingTools` (cancelled/despawned) are dropped.
pub fn collect_tools(
    mut results: ResMut<ToolResults>,
    mut agents: Query<ToolQuery, With<AwaitingTools>>,
    sink: Option<Res<crate::host::WorldEventSink>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    while let Ok(outcome) = results.0.try_recv() {
        let Ok((
            mut window,
            infer,
            routing,
            sensitivities,
            context_results,
            cursor,
            buffer,
            blueprint,
            repetition,
            progress,
            flags,
            activity,
            (metadata, agent_state),
        )) = agents.get_mut(outcome.entity)
        else {
            continue; // stale: agent cancelled/despawned since dispatch
        };
        crate::tick_scope::enter(outcome.entity);
        // Report each lane call's completion before file tracking rewrites
        // successful results. Pairs with `ToolCallStarted` by call id; inline
        // context results (merged below) were never announced and are skipped.
        if let (Some(sink), Some(md), Some(state)) = (sink.as_ref(), metadata, agent_state) {
            for (id, result) in &outcome.results {
                let tool = infer
                    .tool_calls
                    .iter()
                    .find(|c| &c.tool_id == id)
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                let _ = sink.0.send(crate::host::WorldEvent::ToolCallFinished {
                    run_id: md.run_id.clone(),
                    agent_id: state.agent_id.clone(),
                    call_id: id.clone(),
                    tool,
                    ok: !call_had_no_effect(result),
                    summary: one_line(result, 200),
                });
            }
        }
        // Merge the inline context-tool results (if any) with the lane results,
        // ordered by the original tool calls.
        let mut parts = outcome.results;
        if let Some(ctx) = context_results {
            parts.extend(ctx.0.iter().cloned());
        }
        let mut merged = merge_in_call_order(&infer.tool_calls, &parts);
        // Modification accounting (issue #107): count the file-writing calls this
        // stage actually landed, so a `require_modifications` transition gate can
        // tell "analyzed the code and wrote nothing" from "made the change".
        // Done before file tracking, which rewrites successful results.
        record_modifications(
            &infer.tool_calls,
            &merged,
            &stage_modifying_tools(blueprint, cursor),
            progress,
            flags,
        );
        // Record each call for the telemetry observer before file tracking
        // rewrites successful results; success is the `[error] ` result-text
        // convention every executor follows.
        if let Some(mut activity) = activity {
            let batch_latency_ms = u64::try_from(outcome.elapsed.as_millis()).unwrap_or(u64::MAX);
            for (call, (_id, result)) in infer.tool_calls.iter().zip(merged.iter()) {
                activity.0.push(crate::telemetry::ActivityRecord::ToolCall {
                    tool_name: call.name.clone(),
                    batch_latency_ms,
                    success: !result.starts_with("[error]"),
                });
            }
        }
        // File tracking: sync read/write results into the configured HashMap
        // region and replace the inline result with a reference (de-dup context).
        if let Some(ft) = blueprint.and_then(|bp| bp.0.file_tracking.as_ref()) {
            apply_file_tracking(&mut window, ft, &infer.tool_calls, &mut merged);
        }
        // Buffer one readable `[tool] name: result` line per call for the stage's
        // logs (merged is in call order, so it zips with the calls by index).
        if let Some(mut buffer) = buffer {
            let idx = cursor.map_or(0, |c| c.index);
            for (call, (_id, result)) in infer.tool_calls.iter().zip(merged.iter()) {
                buffer.logs.push((
                    idx,
                    format!("[tool] {}: {}", call.name, one_line(result, 200)),
                ));
            }
        }
        apply_tool_results(
            &mut window,
            &infer.response,
            &infer.tool_calls,
            &merged,
            routing.map(|c| &c.routing),
            sensitivities.map(|s| &s.0),
        );
        // Repetition detection: record each call and inject a `[System]` nudge
        // when the agent is looping (same tool+args, or a long read-only streak).
        if let Some(mut detector) = repetition {
            let nudges: Vec<String> = infer
                .tool_calls
                .iter()
                .filter_map(|call| detector.record_call(&call.name, &call.arguments.to_string()))
                .collect();
            for nudge in nudges {
                let content = format!("[System] {nudge}");
                let tokens = leviath_core::estimate_tokens(&content);
                let _ = window.add_to_region("conversation", content, tokens);
            }
        }
        commands
            .entity(outcome.entity)
            .remove::<AwaitingTools>()
            .remove::<ContextToolResults>()
            .remove::<InFlightWork>()
            .insert(ReadyToInfer);
    }
}
