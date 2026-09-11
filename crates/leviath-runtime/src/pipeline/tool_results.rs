//! Applying completed tool batches: results, file tracking, modification accounting.

use super::tools::typed_results;
use super::*;

/// The receiving end of the tool-outcomes channel, as a world resource.
#[derive(Resource)]
pub(crate) struct ToolResults(pub UnboundedReceiver<ToolOutcome>);

/// What a region actually did with a tool result routed into it.
///
/// A routed result leaves a pointer in `conversation` describing where the
/// output went, and the pointer is only worth anything if it is true: the
/// region may have been too full to take the result whole, in which case
/// "stored in region X" is a claim about tokens that are not there.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stored {
    /// The result went in as written.
    Whole,
    /// The region took a prefix; `omitted` characters did not fit.
    Truncated { omitted: usize },
    /// The region refused even a truncated entry; only a marker is there.
    Dropped,
    /// The region's `on_write` hook rejected the write for this reason;
    /// nothing was stored.
    Rejected(String),
}

/// Tools whose first argument is a path into the workspace, and which therefore
/// get the region hint below when that path is not one.
const PATH_TOOLS: [&str; 5] = [
    "read_file",
    "read_files",
    "list_dir",
    "write_file",
    "edit_file",
];

/// Append a corrective hint to a path tool's error when the path was never a
/// path.
///
/// Models routinely aim `read_file` at a context region - `raw_findings`,
/// `sources_index`, `claims` - because the region is a labelled block in their
/// prompt and a file is the only thing they have a read verb for. The tools
/// crate cannot tell them otherwise: it resolves paths and has no view of the
/// context window. So the correction happens here, where the window is in
/// scope, and it names the heading the region is already rendered under.
///
/// Measured on 152 local runs before this existed: 168 of 299 `read_file` calls
/// failed, 90 of them on a region name, spread over 32 of the 46 runs that used
/// the tool at all. One run spent five turns on five spellings of the same
/// region across three stages. A quarter of those came from agents that route
/// nothing and emit no pointer, which is why the fix has to live on the error
/// rather than only on the routing pointer.
pub(crate) fn annotate_path_errors(
    window: &ContextWindow,
    tool_calls: &[crate::components::ToolCall],
    merged: &mut [crate::tool_bridge::ToolResult],
) {
    for (call, (_id, result)) in tool_calls.iter().zip(merged.iter_mut()) {
        if !result.starts_with("[error]") || !PATH_TOOLS.contains(&call.name.as_str()) {
            continue;
        }
        // `read_files` takes `paths`; everything else takes `path`. Either way
        // the last segment is what identifies a region: the model reaches for
        // `raw_findings`, `/context/raw_findings` and `<workdir>/raw_findings`
        // in turn, and all three name the same thing.
        let path = call
            .arguments
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| {
                call.arguments
                    .get("paths")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            });
        let hint = match path.as_deref() {
            Some(p) => region_hint(window, p),
            None => None,
        };
        // "Is a directory" is the other half of the same problem: the model has
        // the right path and the wrong tool, and the OS error does not say so.
        let hint = hint.or_else(|| {
            result.contains("Is a directory").then(|| {
                "That path is a directory - use list_dir to see what is in it.".to_string()
            })
        });
        if let Some(hint) = hint {
            *result = format!("{result} {hint}").into();
        }
    }
}

/// The hint for a path whose last segment names a region this window holds.
fn region_hint(window: &ContextWindow, path: &str) -> Option<String> {
    let leaf = path
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or(path);
    let region = window.regions.iter().find(|r| r.name == leaf)?;
    let name = &region.name;
    match window.hidden.contains(name) {
        false => Some(format!(
            "'{name}' is a context region, not a file - its contents are already in this prompt, \
             under the '{name}' heading. Read them there rather than through a tool."
        )),
        true => Some(format!(
            "'{name}' is a context region rather than a file, and this stage does not carry it, \
             so there is nothing to read here."
        )),
    }
}

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
    tool_results: &[crate::tool_bridge::ToolResult],
    routing: Option<&leviath_core::blueprint::ToolResultRouting>,
    sensitivities: Option<&std::collections::HashMap<String, leviath_core::TaintLevel>>,
    reasoning: Option<String>,
) {
    apply_tool_results_with_parts(
        window,
        Reply {
            text: response_content,
            parts: &[],
            // No produced parts here, so there is nothing to route.
            stage: None,
        },
        tool_calls,
        tool_results,
        routing,
        sensitivities,
        reasoning,
    );
}

/// A reply as the assistant turn records it: its text and any mime the
/// model produced beside its tool calls.
pub(crate) struct Reply<'a> {
    /// The reply's text.
    pub(crate) text: &'a str,
    /// The mime it produced, already stored.
    pub(crate) parts: &'a [leviath_core::mime::Part],
    /// The stage this reply came from, when its `output_routing` should send
    /// some produced parts to regions of their own. `None` keeps every part
    /// in the conversation.
    pub(crate) stage: Option<&'a leviath_core::blueprint::Stage>,
}

/// [`apply_tool_results`] for a reply that produced mime beside its tool
/// calls: the parts ride the assistant turn ahead of the tool results.
pub(crate) fn apply_tool_results_with_parts(
    window: &mut ContextWindow,
    reply: Reply<'_>,
    tool_calls: &[crate::components::ToolCall],
    tool_results: &[crate::tool_bridge::ToolResult],
    routing: Option<&leviath_core::blueprint::ToolResultRouting>,
    sensitivities: Option<&std::collections::HashMap<String, leviath_core::TaintLevel>>,
    reasoning: Option<String>,
) {
    // The stage may route some produced parts to regions of their own
    // (`output_routing`). The assistant turn keeps the reply's text, its
    // unrouted parts and its tool calls; the routed parts land in their
    // regions as separate entries (written after the turn, since they go
    // elsewhere than the conversation).
    let routed = super::part_routing::split(reply.stage, reply.parts);
    let content = super::response::reply_content(reply.text, &routed.kept)
        .unwrap_or_else(|| leviath_core::region::EntryContent::text(reply.text));
    let response_tokens = content.tokens_hint();
    let serialized: Vec<leviath_core::SerializedToolCall> = tool_calls
        .iter()
        .map(|tc| leviath_core::SerializedToolCall {
            id: tc.tool_id.clone(),
            name: tc.name.clone(),
            arguments: tc.arguments.clone(),
            thought_signature: tc.thought_signature.clone(),
        })
        .collect();
    let _ = window.add_assistant_turn_content(
        "conversation",
        leviath_core::EntryKind::AssistantTurn {
            tool_calls: serialized,
        },
        content,
        response_tokens,
        reasoning,
    );
    super::part_routing::store_routed(window, &routed);

    for (tool_call_id, result) in tool_results {
        let tool_name = tool_calls
            .iter()
            .find(|tc| tc.tool_id == *tool_call_id)
            .map(|tc| tc.name.clone())
            .unwrap_or_default();
        apply_one_tool_result(
            window,
            &tool_name,
            tool_call_id,
            result.clone(),
            routing,
            sensitivities,
        );
    }
}

/// Land one tool result: cap it, route it, and leave the pointer that says where
/// it went.
///
/// Split out of [`apply_tool_results`] because a fan-out started from a tool call
/// parks its parent and delivers its result long after the rest of the batch has
/// landed. That result has to be stored exactly as any other - same caps, same
/// routing, same pointer - and a second copy of this logic would not have stayed
/// equal to the first.
pub(crate) fn apply_one_tool_result(
    window: &mut ContextWindow,
    tool_name: &str,
    tool_call_id: &str,
    result: leviath_core::region::EntryContent,
    routing: Option<&leviath_core::blueprint::ToolResultRouting>,
    sensitivities: Option<&std::collections::HashMap<String, leviath_core::TaintLevel>>,
) {
    // The cap below is about text. A stored part is priced by its own
    // estimate and kept whole: cutting an image in half is not a smaller
    // image.
    let stored: Vec<leviath_core::mime::Part> = result.stored().cloned().collect();
    let mut result_text = match stored.is_empty() {
        true => result.into_string(),
        false => result.inline_text(),
    };
    let tool_name = tool_name.to_string();
    let tool_call_id = tool_call_id.to_string();

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
    let result_content = match stored.is_empty() {
        true => leviath_core::region::EntryContent::text(result_text),
        false => {
            let mut parts = vec![leviath_core::mime::Part::text(result_text)];
            parts.extend(stored);
            leviath_core::region::EntryContent::from_parts(parts)
        }
    };
    let result_tokens = result_content.tokens_hint();

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
    //
    // Reports which of the four happened, because the pointer left in the
    // conversation describes this write: it must not promise the full result
    // when the region kept less than that - or nothing at all.
    //
    // `origin` is per write: the routed store is the model's own output
    // landing where the stage routed it (Agent, so a custom region's
    // refusal comes back with its reason), while the conversation entries
    // (the tool_result pairing and the pointer) are framework records a
    // script must not be able to delete (System).
    let add_kind = |window: &mut ContextWindow,
                    region: &str,
                    kind: leviath_core::EntryKind,
                    content: leviath_core::region::EntryContent,
                    tokens: usize,
                    origin: crate::components::WriteOrigin|
     -> Stored {
        let put = |w: &mut ContextWindow, c: leviath_core::region::EntryContent, t: usize| {
            w.typed_write_content(origin, region, kind.clone(), c, t, taint_level)
        };
        match put(window, content.clone(), tokens) {
            Ok(()) => return Stored::Whole,
            Err(leviath_core::Error::RegionRefusedWrite { reason, .. }) => {
                return Stored::Rejected(reason);
            }
            Err(_) => {}
        }
        let available = window
            .get_region(region)
            .map(|r| r.max_tokens.saturating_sub(r.current_tokens))
            .unwrap_or(0);
        let (truncated, omitted) = if available > 100 {
            let char_budget = (available - 10) * 4;
            let prefix = truncate_on_char_boundary(&content, char_budget);
            let omitted = content.len().saturating_sub(prefix.len());
            (
                format!("{}... [truncated, {} chars omitted]", prefix, omitted),
                omitted,
            )
        } else {
            (
                "[tool result truncated - context window full]".to_string(),
                content.len(),
            )
        };
        let trunc_tokens = leviath_core::estimate_tokens(&truncated);
        match put(window, truncated.into(), trunc_tokens) {
            Ok(()) => return Stored::Truncated { omitted },
            // The hook re-ran over the truncated text and refused that shape:
            // still a rejection, not a budget problem.
            Err(leviath_core::Error::RegionRefusedWrite { reason, .. }) => {
                return Stored::Rejected(reason);
            }
            Err(_) => {}
        }
        let _ = put(window, "[result omitted]".into(), 5);
        Stored::Dropped
    };
    let result_kind = || leviath_core::EntryKind::ToolResult {
        tool_call_id: tool_call_id.clone(),
        tool_name: tool_name.clone(),
        is_error: false,
    };

    if target_region == "conversation" {
        // Not routed (or routed back to the message stream): the tool_result
        // lives in `conversation`, paired with its tool_use. System origin -
        // the pairing is required by the provider, so a script cannot refuse it.
        add_kind(
            window,
            "conversation",
            result_kind(),
            result_content,
            result_tokens,
            crate::components::WriteOrigin::System,
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
        let preview: String = result_content.chars().take(160).collect();
        let ellipsis = if result_content.len() > preview.len() {
            "…"
        } else {
            ""
        };
        let hidden = window.hidden.contains(target_region);
        // Stored FIRST, so the pointer can say what actually happened
        // rather than what was intended. The two writes land in different
        // regions, so the tool_use/tool_result adjacency Anthropic requires
        // is unaffected by the order.
        let stored = add_kind(
            window,
            target_region,
            leviath_core::EntryKind::Text,
            result_content,
            result_tokens,
            crate::components::WriteOrigin::Agent,
        );
        // What this text asks the model to do is the whole point of it.
        //
        // "Read that region for the full result" is an instruction with no
        // tool behind it: the region is rendered into the system prompt
        // already, and `context_read` is not granted by most stages that
        // route. Models do the only thing left and point `read_file` at the
        // region name - across 152 local runs, 90 of 168 failed `read_file`
        // calls were a region name where a path belongs, one run spending
        // five turns on five spellings of `raw_findings`. So the pointer
        // names the `## region` heading the assembler emits and says no call
        // is needed.
        //
        // A region this stage does not render is the other half: the model
        // cannot go and read it, so telling it to is worse than saying
        // nothing. `lev validate` refuses a blueprint that routes
        // that way, so reaching here means a layout swapped underneath a
        // routing rule rather than an author mistake - but the model still
        // needs to be told the truth about where its output went.
        let pointer = match (hidden, stored) {
            (true, _) => format!(
                "[output stored in context region '{target_region}' ({result_tokens} tokens), which this stage does not carry - it is kept for a later stage and cannot be read from here. Preview: {preview}{ellipsis}]"
            ),
            (false, Stored::Whole) => format!(
                "[output ({result_tokens} tokens) is in your context under the '{target_region}' heading - it is already in this prompt, so no tool call is needed to see it. Preview: {preview}{ellipsis}]"
            ),
            (false, Stored::Truncated { omitted }) => format!(
                "[output was too large for context region '{target_region}': the start of it is in this prompt under that heading and {omitted} characters were dropped. Release what you are finished with (context_delete) before fetching more this size. Preview: {preview}{ellipsis}]"
            ),
            (false, Stored::Dropped) => format!(
                "[output could NOT be stored - context region '{target_region}' is full and refused it, so only this preview survives. Release what you are finished with (context_delete) and fetch it again if you still need it. Preview: {preview}{ellipsis}]"
            ),
            (false, Stored::Rejected(reason)) => format!(
                "[output was refused by context region '{target_region}': {reason}. Nothing was stored there; only this preview survives. Preview: {preview}{ellipsis}]"
            ),
        };
        let pointer_tokens = leviath_core::estimate_tokens(&pointer);
        add_kind(
            window,
            "conversation",
            result_kind(),
            pointer.into(),
            pointer_tokens,
            crate::components::WriteOrigin::System,
        );
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
    merged: &mut [crate::tool_bridge::ToolResult],
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
            "read_file" if ft.track_reads => (result.as_str().to_string(), "stored"),
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
        )
        .into();
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

/// Tally this batch's `web_search` calls onto the run's outcome flags, counting
/// how many came back with nothing usable.
///
/// "Nothing usable" is an empty JSON array, an empty result, or a bracketed
/// diagnostic - the shape `web_search` returns when it has no engine configured,
/// when the engine errored, or when it fell back to an encyclopedia. All three
/// mean the same thing to the run: this search did not see the web.
///
/// Worth persisting because the failure is otherwise invisible. A model handed
/// an empty result set does not stop; it fills the gap from training data and
/// cites what it remembers, so the run finishes `complete` with a fully cited
/// report resting on nothing. One did that across 47 consecutive failed
/// searches. `searches_empty == searches_run` is the only trace that survives.
pub(crate) fn record_searches(
    tool_calls: &[crate::components::ToolCall],
    merged: &[crate::tool_bridge::ToolResult],
    flags: Option<bevy_ecs::prelude::Mut<'_, crate::persistence::RunOutcomeFlags>>,
) {
    let Some(mut flags) = flags else { return };
    for (call, (_id, result)) in tool_calls.iter().zip(merged.iter()) {
        if leviath_tools::canonical_tool_name(&call.name) != "web_search" {
            continue;
        }
        flags.0.searches_run += 1;
        if search_found_nothing(result) {
            flags.0.searches_empty += 1;
        }
    }
}

/// Whether a `web_search` result carries no usable hits.
///
/// Kept separate from [`record_searches`] because "what an empty search looks
/// like" is the part that changes when the tool script does.
fn search_found_nothing(result: &str) -> bool {
    let trimmed = result.trim();
    // A bracketed opener is the convention every script tool uses for a
    // diagnostic ([error], [denied], and web_search's own prose), and no result
    // set starts that way - a list of hits starts with `[{`.
    trimmed.is_empty()
        || trimmed == "[]"
        || (trimmed.starts_with('[') && !trimmed.starts_with("[{"))
}

/// Tally this batch's file-modifying tool calls onto the stage's progress and the
/// run's outcome flags. A result prefixed `[denied]` (permission layer) counts as
/// *blocked* rather than successful - the agent tried and was refused, which a
/// gate treats differently from never having tried. `[error]` results (the write
/// itself failed) count as neither.
pub(crate) fn record_modifications(
    tool_calls: &[crate::components::ToolCall],
    merged: &[crate::tool_bridge::ToolResult],
    modifying: &[String],
    progress: Option<bevy_ecs::prelude::Mut<'_, StageProgress>>,
    flags: Option<bevy_ecs::prelude::Mut<'_, crate::persistence::RunOutcomeFlags>>,
    workdir: Option<(&str, i64)>,
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
    // A `shell` command names no path and is not a modifying tool, so the loop
    // above never sees the files it wrote. When the batch ran one that landed,
    // scan the working directory for files modified since the run began and
    // fold them into the list, so "what it changed" names the chart the run was
    // for, not only the script that drew it.
    if let (Some(flags), Some((workdir, started_at))) = (flags.as_mut(), workdir) {
        fold_shell_modifications(&mut flags.0, tool_calls, merged, workdir, started_at);
    }
}

/// Fold the files a successful `shell` call left in `workdir` into `flags`, so a
/// creation no modifying tool named is still counted as a change. A no-op when
/// the batch ran no shell that landed. Split out from [`record_modifications`],
/// which holds its state behind ECS `Mut` handles, so the scan is testable on a
/// plain [`RunFlags`](leviath_core::run_meta::RunFlags) and a temp directory.
fn fold_shell_modifications(
    flags: &mut leviath_core::run_meta::RunFlags,
    tool_calls: &[crate::components::ToolCall],
    merged: &[crate::tool_bridge::ToolResult],
    workdir: &str,
    started_at: i64,
) {
    if !batch_ran_shell(tool_calls, merged) {
        return;
    }
    for rel in workdir_modifications_since(
        std::path::Path::new(workdir),
        started_at,
        MAX_SCANNED_MODIFICATIONS,
        MAX_SCAN_DEPTH,
    ) {
        flags.note_modified_path(&rel);
    }
}

/// The most workdir paths one scan folds into `modified_files`; the record cap,
/// so a shell cannot push a run past what a modifying tool could.
const MAX_SCANNED_MODIFICATIONS: usize = leviath_core::run_meta::MAX_TRACKED_MODIFIED_FILES;

/// The deepest a modification scan descends. A shell's output is almost always
/// shallow; this bounds a pathological tree.
const MAX_SCAN_DEPTH: usize = 8;

/// Whether the batch ran a `shell` call that landed (not refused, not an error).
/// Emptiness is not "no effect" here: a silent `python plot.py` writes a file
/// and prints nothing, so any successful shell run earns a scan.
fn batch_ran_shell(
    calls: &[crate::components::ToolCall],
    merged: &[crate::tool_bridge::ToolResult],
) -> bool {
    calls
        .iter()
        .zip(merged.iter())
        .any(|(call, (_id, result))| {
            leviath_tools::canonical_tool_name(&call.name) == "shell"
                && !result.starts_with("[denied]")
                && !result.starts_with("[error]")
        })
}

/// A file's mtime as unix seconds, or 0 when the platform will not say.
fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Working-directory files modified at or after `since` (unix seconds), as
/// paths relative to `workdir`, hidden entries skipped, bounded by `cap`
/// results and `max_depth` levels of recursion. How a `shell` command's
/// creations reach `modified_files`, which the modifying-tool path - keyed on a
/// `path` argument no shell call carries - cannot see.
fn workdir_modifications_since(
    workdir: &std::path::Path,
    since: i64,
    cap: usize,
    max_depth: usize,
) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut stack = vec![(workdir.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if found.len() >= cap {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if found.len() >= cap {
                break;
            }
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue; // hidden files and dirs (.git, .cache) are noise
            }
            let path = entry.path();
            // A metadata failure, a symlink, a socket: contributes nothing and
            // no child, which is the right thing, and folds into the last arm.
            match entry.metadata() {
                Ok(meta) if meta.is_dir() => {
                    if depth < max_depth {
                        stack.push((path, depth + 1));
                    }
                }
                Ok(meta) if meta.is_file() && mtime_secs(&meta) >= since => {
                    let rel = path.strip_prefix(workdir).unwrap_or(&path);
                    found.push(rel.to_string_lossy().replace('\\', "/"));
                }
                _ => {}
            }
        }
    }
    found.sort();
    found
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
pub(crate) fn collect_tools(
    mut results: ResMut<ToolResults>,
    mut agents: Query<ToolQuery, With<AwaitingTools>>,
    // Stage-entry seed batches ride the same lane, so they arrive on the same
    // channel. They are claimed here rather than in a system of their own
    // because a channel has one receiver: a second drainer would take whichever
    // outcomes it happened to reach first, and the other kind would vanish.
    mut seeding: Query<
        (
            &crate::stage_seeds::PendingStageSeeds,
            &mut crate::components::ContextWindow,
        ),
        Without<AwaitingTools>,
    >,
    sink: Option<Res<crate::host::WorldEventSink>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    while let Ok(outcome) = results.0.try_recv() {
        // A seed batch is not a turn: its results fill regions and release the
        // stage, rather than being appended to the conversation as tool results
        // for calls the model never made.
        if let Ok((pending, mut window)) = seeding.get_mut(outcome.entity) {
            crate::tick_scope::enter(outcome.entity);
            crate::stage_seeds::apply_stage_seeds(
                outcome.entity,
                pending,
                &outcome.results,
                &mut window,
                &mut commands,
            );
            continue;
        }
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
            mut flags,
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
            parts.extend(typed_results(&ctx.0));
        }
        let mut merged = merge_in_call_order(&infer.tool_calls, &parts);
        // Modification accounting: count the file-writing calls this
        // stage actually landed, so a `require_modifications` transition gate can
        // tell "analyzed the code and wrote nothing" from "made the change".
        // Done before file tracking, which rewrites successful results.
        // Search accounting: a research run whose every search came back empty
        // still writes a confident report, so the count has to survive the run.
        record_searches(
            &infer.tool_calls,
            &merged,
            flags.as_mut().map(bevy_ecs::prelude::Mut::reborrow),
        );
        record_modifications(
            &infer.tool_calls,
            &merged,
            &stage_modifying_tools(blueprint, cursor),
            progress,
            flags,
            metadata.map(|m| (m.workdir.as_str(), m.started_at)),
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
        // A path tool aimed at a context region fails with an OS error that says
        // nothing about why, and the model tries another spelling. Correct it
        // here, before anything downstream reads the text: after the telemetry
        // and modification passes above, which key off the `[error]` prefix the
        // hint leaves in place, and before file tracking rewrites results.
        annotate_path_errors(&window, &infer.tool_calls, &mut merged);
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
            infer.reasoning.clone(),
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

#[cfg(test)]
mod modification_scan_tests {
    use super::*;
    use crate::components::ToolCall;
    use leviath_core::region::EntryContent;
    use leviath_core::run_meta::RunFlags;

    /// A tool call with a name and no arguments.
    fn call(name: &str) -> ToolCall {
        ToolCall {
            tool_id: format!("id-{name}"),
            name: name.to_string(),
            arguments: serde_json::json!({}),
            thought_signature: None,
        }
    }

    /// A result whose text is `body`.
    fn result(body: &str) -> crate::tool_bridge::ToolResult {
        ("id".to_string(), EntryContent::text(body))
    }

    /// Force a file's mtime to `secs` since the epoch.
    fn set_mtime(path: &std::path::Path, secs: u64) {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    #[test]
    fn the_scan_finds_new_files_skips_old_and_hidden_and_recurses() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // An old input, a new output, a hidden file, and a nested output.
        std::fs::write(root.join("input.csv"), b"old").unwrap();
        set_mtime(&root.join("input.csv"), 1_000);
        std::fs::write(root.join("chart.png"), b"new").unwrap();
        set_mtime(&root.join("chart.png"), 10_000);
        std::fs::write(root.join(".hidden"), b"noise").unwrap();
        set_mtime(&root.join(".hidden"), 10_000);
        std::fs::create_dir(root.join("out")).unwrap();
        std::fs::write(root.join("out/nested.png"), b"new").unwrap();
        set_mtime(&root.join("out/nested.png"), 10_000);

        // Recursing, everything at or after `since` that is not hidden.
        let found = workdir_modifications_since(root, 5_000, 100, 8);
        assert_eq!(found, vec!["chart.png", "out/nested.png"]);

        // Depth 0 does not descend, so the nested output is not found.
        let shallow = workdir_modifications_since(root, 5_000, 100, 0);
        assert_eq!(shallow, vec!["chart.png"]);

        // A missing directory scans to nothing rather than erroring.
        let gone = workdir_modifications_since(&root.join("nope"), 0, 100, 8);
        assert!(gone.is_empty());
    }

    #[test]
    fn the_scan_stops_at_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for i in 0..3 {
            let p = root.join(format!("f{i}.png"));
            std::fs::write(&p, b"x").unwrap();
            set_mtime(&p, 10_000);
        }
        // A positive cap stops the inner loop once it is full.
        assert_eq!(workdir_modifications_since(root, 0, 1, 8).len(), 1);
        // A zero cap stops before reading anything.
        assert!(workdir_modifications_since(root, 0, 0, 8).is_empty());
    }

    #[test]
    fn a_shell_that_landed_earns_a_scan_and_nothing_else_does() {
        assert!(batch_ran_shell(&[call("shell")], &[result("ok")]));
        assert!(!batch_ran_shell(
            &[call("shell")],
            &[result("[denied] refused")]
        ));
        assert!(!batch_ran_shell(
            &[call("shell")],
            &[result("[error] boom")]
        ));
        assert!(!batch_ran_shell(&[call("write_file")], &[result("ok")]));
        assert!(!batch_ran_shell(&[], &[]));
    }

    #[test]
    fn folding_adds_shell_outputs_only_when_a_shell_ran() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let wd = root.to_string_lossy().into_owned();
        std::fs::write(root.join("chart.png"), b"x").unwrap();
        set_mtime(&root.join("chart.png"), 10_000);

        // No shell in the batch: the scan does not run, the list stays empty.
        let mut flags = RunFlags::default();
        fold_shell_modifications(
            &mut flags,
            &[call("write_file")],
            &[result("ok")],
            &wd,
            5_000,
        );
        assert!(flags.modified_files.is_empty());

        // A shell that landed: the created file joins the list, without bumping
        // the modifying-tool-call count.
        let mut flags = RunFlags::default();
        fold_shell_modifications(&mut flags, &[call("shell")], &[result("")], &wd, 5_000);
        assert_eq!(flags.modified_files, vec!["chart.png"]);
        assert_eq!(flags.modified_file_count, 0);
    }
}
