//! Runtime side of script-backed custom regions (`RegionKind::Custom`).
//!
//! The scripting layer ([`leviath_scripting::region_hook`]) compiles and runs
//! the hooks; this module owns everything on the runtime side of that JSON
//! boundary: building the `ctx` objects a hook receives, interpreting each
//! hook's returned value, and every fallback. The contract is that a hook
//! failure can never fail an inference or lose a write:
//!
//! - `render` failure → the region renders as a Temporary-style block
//!   (`[{name}]:\n…`, `Never` cache hint) and a warning names the script.
//! - `on_write` failure → the entry is accepted unchanged.
//! - `on_overflow` failure or invalid indices → oldest-first eviction.

use std::sync::Arc;

use leviath_core::{EntryKind, Region, RegionEntry};
use leviath_scripting::region_hook::{RegionScript, run_on_overflow, run_on_write, run_render};

/// Stage-level metadata threaded into `render(ctx)`. `Default` (empty name,
/// zero iterations, empty model) is used by callers with no stage context -
/// the transition-choice request and plain `assemble()` in tests.
#[derive(Debug, Clone, Default)]
pub struct AssembleMeta {
    /// Current stage name (`AgentState::current_stage`).
    pub stage_name: String,
    /// Inference count within the current stage (`StageProgress::iterations`).
    pub stage_iterations: usize,
    /// Model id serving this stage.
    pub model: String,
}

/// What `on_write` decided about an incoming entry.
pub(crate) enum OnWriteOutcome {
    /// Store the entry with this (possibly replaced) content and token count.
    Accept(String, usize),
    /// The script declined the entry; report success to the writer.
    Drop,
}

/// Serialize one region entry for a hook ctx. Typed metadata crosses as plain
/// data so a script can key decisions off tool ids/names, but it is read-only:
/// hooks return instructions (drop indices, replacement text), never entries.
fn entry_to_json(entry: &RegionEntry) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "content": entry.content(),
        "tokens": entry.tokens,
        "timestamp": entry.timestamp,
        "key": entry.key,
    });
    let (kind, extra) = match &entry.kind {
        EntryKind::Text => ("text", None),
        EntryKind::UserMessage => ("user_message", None),
        EntryKind::AssistantTurn { tool_calls } => (
            "assistant_turn",
            Some((
                "tool_calls",
                serde_json::to_value(tool_calls).unwrap_or_default(),
            )),
        ),
        EntryKind::ToolResult {
            tool_call_id,
            tool_name,
            is_error,
        } => {
            obj["tool_call_id"] = serde_json::json!(tool_call_id);
            obj["tool_name"] = serde_json::json!(tool_name);
            obj["is_error"] = serde_json::json!(is_error);
            ("tool_result", None)
        }
    };
    obj["kind"] = serde_json::json!(kind);
    if let Some((k, v)) = extra {
        obj[k] = v;
    }
    obj
}

/// The `region` sub-object shared by all three hook ctx shapes.
fn region_to_json(region: &Region) -> serde_json::Value {
    serde_json::json!({
        "name": region.name,
        "budget": region.max_tokens,
        "current_tokens": region.current_tokens,
        "entry_count": region.content.len(),
    })
}

/// The Temporary-style block a custom region falls back to whenever its hook
/// can't run or misbehaves - identical to the plain-`assemble` arm, so the
/// region is never silently dropped.
fn fallback_block(region: &Region) -> leviath_providers::SystemBlock {
    let text = region
        .content
        .iter()
        .map(|e| e.content())
        .collect::<Vec<_>>()
        .join("\n\n");
    leviath_providers::SystemBlock {
        text: format!("[{}]:\n{}", region.name, text),
        cache_hint: leviath_core::CacheHint::Never,
    }
}

/// Render a custom region through its script, appending the results to the
/// caller's block/message accumulators. Any failure falls back to the
/// Temporary-style block with a warning; success is followed by a warn-only
/// token re-check against the region's budget (no truncation - the opt-in
/// exact-token preflight remains the hard guard).
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_custom_region(
    region: &Region,
    script: Option<&Arc<RegionScript>>,
    persistent: bool,
    meta: &AssembleMeta,
    window_current: usize,
    window_max: usize,
    system_blocks: &mut Vec<leviath_providers::SystemBlock>,
    messages: &mut Vec<leviath_providers::Message>,
) {
    let Some(script) = script else {
        // No compiled script on the window (plain `assemble()` callers, or a
        // spawn path that skipped resolution). Same shape as a hook failure.
        if !region.content.is_empty() {
            tracing::warn!(
                region = %region.name,
                "custom region has no compiled script; rendering fallback block"
            );
            system_blocks.push(fallback_block(region));
        }
        return;
    };

    let ctx = serde_json::json!({
        "region": region_to_json(region),
        "entries": region.content.iter().map(entry_to_json).collect::<Vec<_>>(),
        "stage_name": meta.stage_name,
        "stage_iterations": meta.stage_iterations,
        "model": meta.model,
        "window": { "total_tokens": window_current, "max_tokens": window_max },
    });

    let rendered = match run_render(script, ctx) {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!(
                region = %region.name,
                script = %script.path,
                error = %e,
                "custom region render failed; using fallback block"
            );
            if !region.content.is_empty() {
                system_blocks.push(fallback_block(region));
            }
            return;
        }
    };

    match parse_render_output(&rendered, persistent) {
        Ok((blocks, msgs)) => {
            let emitted_tokens: usize = blocks
                .iter()
                .map(|b| leviath_core::estimate_tokens(&b.text))
                .chain(msgs.iter().map(|m| {
                    match &m.content {
                        leviath_providers::MessageContent::Text(t) => {
                            leviath_core::estimate_tokens(t)
                        }
                        leviath_providers::MessageContent::Blocks(bs) => bs
                            .iter()
                            .map(|b| match b {
                                leviath_providers::ContentBlock::Text { text } => {
                                    leviath_core::estimate_tokens(text)
                                }
                                leviath_providers::ContentBlock::ToolUse { input, .. } => {
                                    leviath_core::estimate_tokens(&input.to_string())
                                }
                                leviath_providers::ContentBlock::ToolResult { content, .. } => {
                                    leviath_core::estimate_tokens(content)
                                }
                            })
                            .sum(),
                    }
                }))
                .sum();
            if emitted_tokens > region.max_tokens {
                tracing::warn!(
                    region = %region.name,
                    script = %script.path,
                    emitted_tokens,
                    budget = region.max_tokens,
                    "custom region render exceeds its budget; sending anyway \
                     (enable exact_token_counting for a hard guard)"
                );
            }
            system_blocks.extend(blocks);
            messages.extend(msgs);
        }
        Err(reason) => {
            tracing::warn!(
                region = %region.name,
                script = %script.path,
                reason = %reason,
                "custom region render returned an invalid shape; using fallback block"
            );
            if !region.content.is_empty() {
                system_blocks.push(fallback_block(region));
            }
        }
    }
}

/// Interpret `render`'s returned value: a string (one system block) or an
/// object with optional `system` (string or array of strings) and `messages`
/// (array of message objects). Strict on shape - any surprise is an `Err`,
/// which the caller turns into the fallback block.
fn parse_render_output(
    value: &serde_json::Value,
    persistent: bool,
) -> Result<
    (
        Vec<leviath_providers::SystemBlock>,
        Vec<leviath_providers::Message>,
    ),
    String,
> {
    // A persistent region's rendered output is expected stable → cacheable.
    let hint = if persistent {
        leviath_core::CacheHint::Always
    } else {
        leviath_core::CacheHint::UntilChanged
    };
    let block = |text: &str| leviath_providers::SystemBlock {
        text: text.to_string(),
        cache_hint: hint,
    };

    match value {
        serde_json::Value::String(s) => {
            let blocks = if s.is_empty() { vec![] } else { vec![block(s)] };
            Ok((blocks, vec![]))
        }
        serde_json::Value::Object(obj) => {
            let mut blocks = Vec::new();
            match obj.get("system") {
                None | Some(serde_json::Value::Null) => {}
                Some(serde_json::Value::String(s)) => {
                    if !s.is_empty() {
                        blocks.push(block(s));
                    }
                }
                Some(serde_json::Value::Array(items)) => {
                    for item in items {
                        match item {
                            serde_json::Value::String(s) if !s.is_empty() => blocks.push(block(s)),
                            serde_json::Value::String(_) => {}
                            other => {
                                return Err(format!(
                                    "system array items must be strings, found {other}"
                                ));
                            }
                        }
                    }
                }
                Some(other) => {
                    return Err(format!(
                        "system must be a string or array of strings, found {other}"
                    ));
                }
            }
            let mut messages = Vec::new();
            match obj.get("messages") {
                None | Some(serde_json::Value::Null) => {}
                Some(serde_json::Value::Array(items)) => {
                    for item in items {
                        messages.push(message_from_json(item)?);
                    }
                }
                Some(other) => return Err(format!("messages must be an array, found {other}")),
            }
            Ok((blocks, messages))
        }
        other => Err(format!(
            "render must return a string or #{{ system, messages }} map, found {other}"
        )),
    }
}

/// Build one provider message from a script-emitted message object. Three
/// accepted shapes, constructed with the same wire types the built-in
/// SlidingWindow arm emits (so a Rhai recreation of it is byte-identical):
///
/// - `{ role, content }` - plain text, role `user` or `assistant`
/// - `{ role: "assistant", content?, tool_calls: [{id, name, arguments}] }`
/// - `{ role: "user", tool_results: [{tool_call_id, content, is_error?}] }`
fn message_from_json(value: &serde_json::Value) -> Result<leviath_providers::Message, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| format!("each message must be a map, found {value}"))?;
    let role = obj
        .get("role")
        .and_then(|r| r.as_str())
        .ok_or("each message needs a role of \"user\" or \"assistant\"")?;
    if role != "user" && role != "assistant" {
        return Err(format!(
            "message role must be user or assistant, found {role}"
        ));
    }

    let content_str = match obj.get("content") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(other) => return Err(format!("message content must be a string, found {other}")),
    };

    if let Some(calls) = obj.get("tool_calls") {
        if role != "assistant" {
            return Err("tool_calls are only valid on an assistant message".to_string());
        }
        let calls = calls
            .as_array()
            .ok_or_else(|| format!("tool_calls must be an array, found {calls}"))?;
        let mut blocks = Vec::new();
        if let Some(text) = content_str.filter(|s| !s.is_empty()) {
            blocks.push(leviath_providers::ContentBlock::Text { text });
        }
        for call in calls {
            let call = call
                .as_object()
                .ok_or_else(|| format!("each tool_call must be a map, found {call}"))?;
            let id = call
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or("each tool_call needs a string id")?;
            let name = call
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or("each tool_call needs a string name")?;
            blocks.push(leviath_providers::ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: call
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(Default::default())),
                thought_signature: call
                    .get("thought_signature")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            });
        }
        return Ok(leviath_providers::Message {
            role: "assistant".to_string(),
            content: leviath_providers::MessageContent::Blocks(blocks),
            cache_breakpoint: false,
        });
    }

    if let Some(results) = obj.get("tool_results") {
        if role != "user" {
            return Err("tool_results are only valid on a user message".to_string());
        }
        let results = results
            .as_array()
            .ok_or_else(|| format!("tool_results must be an array, found {results}"))?;
        let mut blocks = Vec::new();
        for result in results {
            let result = result
                .as_object()
                .ok_or_else(|| format!("each tool_result must be a map, found {result}"))?;
            let id = result
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .ok_or("each tool_result needs a string tool_call_id")?;
            let content = result
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or("each tool_result needs string content")?;
            blocks.push(leviath_providers::ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: content.to_string(),
                is_error: result
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            });
        }
        return Ok(leviath_providers::Message {
            role: "user".to_string(),
            content: leviath_providers::MessageContent::Blocks(blocks),
            cache_breakpoint: false,
        });
    }

    let content =
        content_str.ok_or("a message without tool_calls/tool_results needs string content")?;
    Ok(leviath_providers::Message {
        role: role.to_string(),
        content: content.into(),
        cache_breakpoint: false,
    })
}

/// Run `on_write` for an entry headed into a custom region. Failure of any
/// kind accepts the entry unchanged - a script bug must not lose writes.
pub(crate) fn apply_on_write(
    script: &RegionScript,
    region: &Region,
    content: String,
    tokens: usize,
    kind: &EntryKind,
) -> OnWriteOutcome {
    let kind_str = match kind {
        EntryKind::Text => "text",
        EntryKind::UserMessage => "user_message",
        EntryKind::AssistantTurn { .. } => "assistant_turn",
        EntryKind::ToolResult { .. } => "tool_result",
    };
    let ctx = serde_json::json!({
        "region": region_to_json(region),
        "entry": { "content": content, "kind": kind_str, "tokens": tokens },
    });
    match run_on_write(script, ctx) {
        Ok(serde_json::Value::String(replacement)) => {
            let tokens = leviath_core::estimate_tokens(&replacement);
            OnWriteOutcome::Accept(replacement, tokens)
        }
        Ok(serde_json::Value::Bool(false)) => OnWriteOutcome::Drop,
        Ok(serde_json::Value::Bool(true)) | Ok(serde_json::Value::Null) => {
            OnWriteOutcome::Accept(content, tokens)
        }
        Ok(other) => {
            tracing::warn!(
                region = %region.name,
                script = %script.path,
                returned = %other,
                "on_write must return a string, true/false, or unit; accepting entry unchanged"
            );
            OnWriteOutcome::Accept(content, tokens)
        }
        Err(e) => {
            tracing::warn!(
                region = %region.name,
                script = %script.path,
                error = %e,
                "on_write failed; accepting entry unchanged"
            );
            OnWriteOutcome::Accept(content, tokens)
        }
    }
}

/// Ask `on_overflow` which entries to drop, validate the answer, and apply it.
/// Returns the tokens freed (0 when the hook is absent, fails, or returns an
/// invalid/empty answer - callers fall back to oldest-first for the rest).
pub(crate) fn apply_overflow(
    script: &RegionScript,
    region: &mut Region,
    needed_tokens: usize,
) -> usize {
    let ctx = serde_json::json!({
        "region": region_to_json(region),
        "entries": region.content.iter().map(entry_to_json).collect::<Vec<_>>(),
        "needed_tokens": needed_tokens,
    });
    let value = match run_on_overflow(script, ctx) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                region = %region.name,
                script = %script.path,
                error = %e,
                "on_overflow failed; falling back to oldest-first eviction"
            );
            return 0;
        }
    };
    let Some(indices) = valid_drop_indices(&value, region.content.len()) else {
        tracing::warn!(
            region = %region.name,
            script = %script.path,
            returned = %value,
            "on_overflow must return an array of in-range entry indices; \
             falling back to oldest-first eviction"
        );
        return 0;
    };

    let mut freed = 0;
    // Descending order keeps earlier indices valid while removing.
    for index in indices.into_iter().rev() {
        let entry = region.content.remove(index);
        freed += entry.tokens;
    }
    region.current_tokens = region.current_tokens.saturating_sub(freed);
    freed
}

/// Validate an `on_overflow` return value into a sorted, deduped index list.
/// `None` when the shape is wrong or any index is out of range.
fn valid_drop_indices(value: &serde_json::Value, len: usize) -> Option<Vec<usize>> {
    let items = value.as_array()?;
    let mut indices = Vec::with_capacity(items.len());
    for item in items {
        let index = item.as_u64()? as usize;
        if index >= len {
            return None;
        }
        indices.push(index);
    }
    indices.sort_unstable();
    indices.dedup();
    Some(indices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_tracing;
    use leviath_core::RegionKind;
    use leviath_scripting::region_hook::compile;
    use serde_json::json;

    fn script(src: &str) -> Arc<RegionScript> {
        Arc::new(compile("test.rhai", src).unwrap())
    }

    fn region_with(entries: &[(&str, EntryKind)]) -> Region {
        let mut region = Region::new(
            "brain".to_string(),
            RegionKind::Custom {
                script: "test.rhai".to_string(),
                persistent: false,
            },
            1000,
        );
        for (content, kind) in entries {
            region.add_typed_entry(*content, 10, kind.clone()).unwrap();
        }
        region
    }

    fn render(
        region: &Region,
        script: Option<&Arc<RegionScript>>,
        persistent: bool,
    ) -> (
        Vec<leviath_providers::SystemBlock>,
        Vec<leviath_providers::Message>,
    ) {
        let mut blocks = Vec::new();
        let mut messages = Vec::new();
        with_tracing(|| {
            render_custom_region(
                region,
                script,
                persistent,
                &AssembleMeta {
                    stage_name: "plan".to_string(),
                    stage_iterations: 2,
                    model: "m1".to_string(),
                },
                50,
                2000,
                &mut blocks,
                &mut messages,
            )
        });
        (blocks, messages)
    }

    // ─── entry_to_json ───────────────────────────────────────────────────

    #[test]
    fn entry_to_json_serializes_all_kinds() {
        let mut region = region_with(&[
            ("plain", EntryKind::Text),
            ("hi", EntryKind::UserMessage),
            (
                "calling",
                EntryKind::AssistantTurn {
                    tool_calls: vec![leviath_core::SerializedToolCall {
                        id: "c1".to_string(),
                        name: "shell".to_string(),
                        arguments: json!({"command": "ls"}),
                        thought_signature: None,
                    }],
                },
            ),
            (
                "result",
                EntryKind::ToolResult {
                    tool_call_id: "c1".to_string(),
                    tool_name: "shell".to_string(),
                    is_error: true,
                },
            ),
        ]);
        region.content[0].key = Some("k".to_string());

        let entries: Vec<_> = region.content.iter().map(entry_to_json).collect();
        assert_eq!(entries[0]["kind"], json!("text"));
        assert_eq!(entries[0]["key"], json!("k"));
        assert_eq!(entries[0]["tokens"], json!(10));
        assert_eq!(entries[1]["kind"], json!("user_message"));
        assert_eq!(entries[2]["kind"], json!("assistant_turn"));
        assert_eq!(entries[2]["tool_calls"][0]["id"], json!("c1"));
        assert_eq!(entries[3]["kind"], json!("tool_result"));
        assert_eq!(entries[3]["tool_call_id"], json!("c1"));
        assert_eq!(entries[3]["is_error"], json!(true));
    }

    // ─── render: happy paths ─────────────────────────────────────────────

    #[test]
    fn render_string_becomes_one_block_with_persistence_hint() {
        let region = region_with(&[("x", EntryKind::Text)]);
        let s = script("fn render(ctx) { `<${ctx.region.name}>` }");

        let (blocks, messages) = render(&region, Some(&s), false);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "<brain>");
        assert_eq!(blocks[0].cache_hint, leviath_core::CacheHint::UntilChanged);
        assert!(messages.is_empty());

        let (blocks, _) = render(&region, Some(&s), true);
        assert_eq!(blocks[0].cache_hint, leviath_core::CacheHint::Always);
    }

    #[test]
    fn render_map_emits_system_array_and_typed_messages() {
        // A script that recreates the SlidingWindow wire shapes: assistant
        // text+tool_use, then a user tool_result message - built-ins parity.
        let src = r#"
            fn render(ctx) {
                #{
                    system: ["s1", "", "s2"],
                    messages: [
                        #{ role: "user", content: "hello" },
                        #{ role: "assistant", content: "thinking", tool_calls: [
                            #{ id: "c1", name: "shell", arguments: #{ command: "ls" } },
                        ] },
                        #{ role: "user", tool_results: [
                            #{ tool_call_id: "c1", content: "file_a", is_error: false },
                        ] },
                    ],
                }
            }
        "#;
        let region = region_with(&[("x", EntryKind::Text)]);
        let (blocks, messages) = render(&region, Some(&script(src)), false);

        assert_eq!(
            blocks.iter().map(|b| b.text.as_str()).collect::<Vec<_>>(),
            vec!["s1", "s2"],
            "empty system strings are skipped"
        );
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "user");
        // Assert the wire shapes through serde - no enum destructuring, so
        // there are no never-taken match arms for the coverage gate.
        let assistant = serde_json::to_value(&messages[1].content).unwrap();
        assert_eq!(assistant[0], json!({ "type": "text", "text": "thinking" }));
        assert_eq!(assistant[1]["type"], json!("tool_use"));
        assert_eq!(assistant[1]["id"], json!("c1"));
        assert_eq!(assistant[1]["name"], json!("shell"));
        let results = serde_json::to_value(&messages[2].content).unwrap();
        assert_eq!(results[0]["type"], json!("tool_result"));
        assert_eq!(results[0]["tool_use_id"], json!("c1"));
        assert_eq!(results[0]["content"], json!("file_a"));
        assert_eq!(results[0]["is_error"], json!(false));
    }

    #[test]
    fn render_map_accepts_single_system_string_and_null_fields() {
        let src = r#"fn render(ctx) { #{ system: "solo", messages: () } }"#;
        let region = region_with(&[("x", EntryKind::Text)]);
        let (blocks, messages) = render(&region, Some(&script(src)), false);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "solo");
        assert!(messages.is_empty());
    }

    #[test]
    fn render_empty_map_and_empty_string_emit_nothing() {
        let region = region_with(&[("x", EntryKind::Text)]);
        for src in ["fn render(ctx) { #{} }", "fn render(ctx) { \"\" }"] {
            let (blocks, messages) = render(&region, Some(&script(src)), false);
            assert!(blocks.is_empty(), "src: {src}");
            assert!(messages.is_empty());
        }
    }

    #[test]
    fn render_sees_stage_meta_and_window_fields() {
        let src = r#"
            fn render(ctx) {
                `${ctx.stage_name}|${ctx.stage_iterations}|${ctx.model}|${ctx.window.total_tokens}|${ctx.window.max_tokens}`
            }
        "#;
        let region = region_with(&[("x", EntryKind::Text)]);
        let (blocks, _) = render(&region, Some(&script(src)), false);
        assert_eq!(blocks[0].text, "plan|2|m1|50|2000");
    }

    #[test]
    fn render_over_budget_warns_but_still_emits() {
        // Budget is 1000 tokens; the script emits ~2000 tokens of output. The
        // result is kept (warn-only per the design), not truncated.
        let src = r#"fn render(ctx) { let s = "x"; s.pad(8000, 'x'); s }"#;
        let region = region_with(&[("x", EntryKind::Text)]);
        let (blocks, _) = render(&region, Some(&script(src)), false);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text.len(), 8000);
    }

    // ─── render: fallbacks ───────────────────────────────────────────────

    #[test]
    fn render_missing_script_falls_back_to_temporary_style() {
        let region = region_with(&[("a", EntryKind::Text), ("b", EntryKind::Text)]);
        let (blocks, messages) = render(&region, None, false);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "[brain]:\na\n\nb");
        assert_eq!(blocks[0].cache_hint, leviath_core::CacheHint::Never);
        assert!(messages.is_empty());
    }

    #[test]
    fn render_missing_script_on_empty_region_emits_nothing() {
        let region = region_with(&[]);
        let (blocks, messages) = render(&region, None, false);
        assert!(blocks.is_empty());
        assert!(messages.is_empty());
    }

    #[test]
    fn render_runtime_error_falls_back() {
        let region = region_with(&[("kept", EntryKind::Text)]);
        let s = script("fn render(ctx) { throw \"broken\" }");
        let (blocks, _) = render(&region, Some(&s), false);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "[brain]:\nkept");
        assert_eq!(blocks[0].cache_hint, leviath_core::CacheHint::Never);
    }

    #[test]
    fn render_error_on_empty_region_emits_nothing() {
        let region = region_with(&[]);
        let s = script("fn render(ctx) { throw \"broken\" }");
        let (blocks, _) = render(&region, Some(&s), false);
        assert!(blocks.is_empty());
    }

    #[test]
    fn render_invalid_shapes_fall_back() {
        let region = region_with(&[("kept", EntryKind::Text)]);
        for src in [
            "fn render(ctx) { 42 }",
            "fn render(ctx) { true }",
            "fn render(ctx) { [1, 2] }",
            "fn render(ctx) { }",
            "fn render(ctx) { #{ system: 42 } }",
            "fn render(ctx) { #{ system: [1] } }",
            "fn render(ctx) { #{ messages: \"not an array\" } }",
            "fn render(ctx) { #{ messages: [42] } }",
            "fn render(ctx) { #{ messages: [#{ content: \"no role\" }] } }",
            "fn render(ctx) { #{ messages: [#{ role: \"system\", content: \"bad role\" }] } }",
            "fn render(ctx) { #{ messages: [#{ role: \"user\", content: 42 }] } }",
            "fn render(ctx) { #{ messages: [#{ role: \"user\" }] } }",
            "fn render(ctx) { #{ messages: [#{ role: \"user\", tool_calls: [] }] } }",
            "fn render(ctx) { #{ messages: [#{ role: \"assistant\", tool_calls: 42 }] } }",
            "fn render(ctx) { #{ messages: [#{ role: \"assistant\", tool_calls: [42] }] } }",
            "fn render(ctx) { #{ messages: [#{ role: \"assistant\", tool_calls: [#{ name: \"n\" }] }] } }",
            "fn render(ctx) { #{ messages: [#{ role: \"assistant\", tool_calls: [#{ id: \"i\" }] }] } }",
            "fn render(ctx) { #{ messages: [#{ role: \"assistant\", tool_results: [] }] } }",
            "fn render(ctx) { #{ messages: [#{ role: \"user\", tool_results: 42 }] } }",
            "fn render(ctx) { #{ messages: [#{ role: \"user\", tool_results: [42] }] } }",
            "fn render(ctx) { #{ messages: [#{ role: \"user\", tool_results: [#{ content: \"c\" }] }] } }",
            "fn render(ctx) { #{ messages: [#{ role: \"user\", tool_results: [#{ tool_call_id: \"i\" }] }] } }",
        ] {
            let (blocks, messages) = render(&region, Some(&script(src)), false);
            assert_eq!(blocks.len(), 1, "src must fall back: {src}");
            assert_eq!(blocks[0].text, "[brain]:\nkept", "src: {src}");
            assert!(messages.is_empty(), "src: {src}");
        }
    }

    #[test]
    fn render_invalid_shape_on_empty_region_emits_nothing() {
        // The invalid-shape fallback has nothing to fall back TO when the
        // region is empty - no block at all.
        let region = region_with(&[]);
        let (blocks, messages) = render(&region, Some(&script("fn render(ctx) { 42 }")), false);
        assert!(blocks.is_empty());
        assert!(messages.is_empty());
    }

    #[test]
    fn render_empty_single_system_string_is_skipped() {
        let src = r#"fn render(ctx) { #{ system: "" } }"#;
        let region = region_with(&[("x", EntryKind::Text)]);
        let (blocks, messages) = render(&region, Some(&script(src)), false);
        assert!(blocks.is_empty());
        assert!(messages.is_empty());
    }

    #[test]
    fn render_tool_call_passes_thought_signature_through() {
        let src = r#"
            fn render(ctx) {
                #{ messages: [#{ role: "assistant", tool_calls: [
                    #{ id: "c", name: "n", thought_signature: "sig123" },
                ] }] }
            }
        "#;
        let region = region_with(&[("x", EntryKind::Text)]);
        let (_, messages) = render(&region, Some(&script(src)), false);
        let blocks = serde_json::to_value(&messages[0].content).unwrap();
        assert_eq!(blocks[0]["thought_signature"], json!("sig123"));
    }

    #[test]
    fn on_write_ctx_reports_every_entry_kind() {
        // The kind string the script sees matches the entry's typed kind.
        let src = r#"
            fn render(ctx) { "" }
            fn on_write(ctx) { ctx.entry.kind }
        "#;
        for (kind, expected) in [
            (EntryKind::Text, "text"),
            (EntryKind::UserMessage, "user_message"),
            (
                EntryKind::AssistantTurn { tool_calls: vec![] },
                "assistant_turn",
            ),
            (
                EntryKind::ToolResult {
                    tool_call_id: "c".to_string(),
                    tool_name: "t".to_string(),
                    is_error: false,
                },
                "tool_result",
            ),
        ] {
            let replaced = on_write_kind(src, "x", &kind);
            assert_eq!(
                replaced.map(|(content, _)| content),
                Some(expected.to_string())
            );
        }
    }

    #[test]
    fn render_assistant_tool_call_defaults_arguments_and_signature() {
        let src = r#"
            fn render(ctx) {
                #{ messages: [#{ role: "assistant", tool_calls: [#{ id: "c", name: "n" }] }] }
            }
        "#;
        let region = region_with(&[("x", EntryKind::Text)]);
        let (_, messages) = render(&region, Some(&script(src)), false);
        let blocks = serde_json::to_value(&messages[0].content).unwrap();
        assert_eq!(blocks[0]["type"], json!("tool_use"));
        assert_eq!(blocks[0]["input"], json!({}));
        // Indexing a missing key yields Null, so this covers absent-or-null
        // without a short-circuit branch the coverage gate can't see taken.
        assert_eq!(blocks[0]["thought_signature"], serde_json::Value::Null);
    }

    // ─── on_write ────────────────────────────────────────────────────────

    /// Run `apply_on_write` and collapse the outcome to an Option - both
    /// enum arms are exercised across this suite (through this one shared
    /// mapping), so it has no never-taken branch.
    fn on_write_kind(src: &str, content: &str, kind: &EntryKind) -> Option<(String, usize)> {
        let region = region_with(&[]);
        let outcome =
            with_tracing(|| apply_on_write(&script(src), &region, content.to_string(), 5, kind));
        match outcome {
            OnWriteOutcome::Accept(content, tokens) => Some((content, tokens)),
            OnWriteOutcome::Drop => None,
        }
    }

    fn on_write_of(src: &str, content: &str) -> Option<(String, usize)> {
        on_write_kind(src, content, &EntryKind::Text)
    }

    #[test]
    fn on_write_replaces_accepts_and_drops() {
        let replaced = on_write_of(
            "fn render(ctx) { \"\" }\nfn on_write(ctx) { ctx.entry.content.to_upper() }",
            "hi",
        );
        assert_eq!(
            replaced,
            Some(("HI".to_string(), leviath_core::estimate_tokens("HI")))
        );

        for accept_body in ["true", ""] {
            let src = format!("fn render(ctx) {{ \"\" }}\nfn on_write(ctx) {{ {accept_body} }}");
            assert_eq!(
                on_write_of(&src, "orig"),
                Some(("orig".to_string(), 5)),
                "body {accept_body:?} accepts unchanged with original tokens"
            );
        }

        assert_eq!(
            on_write_of("fn render(ctx) { \"\" }\nfn on_write(ctx) { false }", "x"),
            None,
            "false drops the entry"
        );
    }

    #[test]
    fn on_write_invalid_return_and_error_accept_unchanged() {
        for src in [
            "fn render(ctx) { \"\" }\nfn on_write(ctx) { 42 }",
            "fn render(ctx) { \"\" }\nfn on_write(ctx) { throw \"bad\" }",
        ] {
            assert_eq!(
                on_write_of(src, "keep"),
                Some(("keep".to_string(), 5)),
                "src: {src}"
            );
        }
    }

    // ─── on_overflow / apply_overflow ────────────────────────────────────

    #[test]
    fn apply_overflow_drops_chosen_indices() {
        let mut region = region_with(&[
            ("a", EntryKind::Text),
            ("b", EntryKind::Text),
            ("c", EntryKind::Text),
        ]);
        // Duplicate + unordered indices are deduped and applied safely.
        let s = script("fn render(ctx) { \"\" }\nfn on_overflow(ctx) { [2, 0, 2] }");
        let freed = with_tracing(|| apply_overflow(&s, &mut region, 15));
        assert_eq!(freed, 20);
        assert_eq!(region.content.len(), 1);
        assert_eq!(region.content[0].content(), "b");
        assert_eq!(region.current_tokens, 10);
    }

    #[test]
    fn apply_overflow_error_and_invalid_shapes_free_nothing() {
        for src in [
            "fn render(ctx) { \"\" }\nfn on_overflow(ctx) { throw \"bad\" }",
            "fn render(ctx) { \"\" }\nfn on_overflow(ctx) { \"not an array\" }",
            "fn render(ctx) { \"\" }\nfn on_overflow(ctx) { [\"x\"] }",
            "fn render(ctx) { \"\" }\nfn on_overflow(ctx) { [99] }",
        ] {
            let mut region = region_with(&[("a", EntryKind::Text)]);
            let freed = with_tracing(|| apply_overflow(&script(src), &mut region, 5));
            assert_eq!(freed, 0, "src: {src}");
            assert_eq!(region.content.len(), 1, "content untouched: {src}");
        }
    }

    #[test]
    fn overflow_ctx_carries_needed_tokens_and_entries() {
        let src = r#"
            fn render(ctx) { "" }
            fn on_overflow(ctx) {
                if ctx.needed_tokens == 7 && ctx.entries.len() == 2 { [0] } else { [] }
            }
        "#;
        let mut region = region_with(&[("a", EntryKind::Text), ("b", EntryKind::Text)]);
        let freed = with_tracing(|| apply_overflow(&script(src), &mut region, 7));
        assert_eq!(freed, 10);
    }
}
