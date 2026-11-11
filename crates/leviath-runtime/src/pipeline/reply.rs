//! A reply with no tool calls, recorded in the conversation as the model's
//! turn.

use crate::components::ContextWindow;

/// Record a reply with no tool calls in the conversation as the model's
/// turn: its text and whatever mime it produced. A reply with nothing in it
/// (a cut-off tool call, an empty answer) leaves no entry: an empty
/// assistant message is noise to the next request and some providers refuse
/// it outright.
pub(super) fn store_reply(
    window: &mut ContextWindow,
    infer: &crate::components::InferenceResult,
    reasoning: Option<String>,
    stage: Option<&crate::spec::graph::StageDef>,
    sink: Option<&crate::context_setup::PartSink<'_>>,
) {
    // The stage may send some produced parts to regions of their own
    // (`output_routing`). The reply's text and any unrouted part stay in the
    // conversation as the assistant turn; the routed parts land in their
    // regions as separate entries.
    let routed = super::part_routing::split(stage, &infer.parts);
    if let Some(content) = reply_content(&infer.response, &routed.kept, sink) {
        let tokens = content.tokens(sink.map(|s| s.registry));
        let _ = window.add_turn(
            Some(leviath_core::ContextCause::ModelReply),
            "conversation",
            leviath_core::EntryKind::AssistantTurn { tool_calls: vec![] },
            content,
            tokens,
            reasoning,
        );
    }
    super::part_routing::store_routed(window, &routed);
}

/// A reply's text and produced parts as one entry's content, or `None` when
/// there is nothing to record. Text over `[mime] inline_text_bytes` is stored
/// through `sink` and the entry carries its stand-in; with no sink it stays
/// inline.
pub(crate) fn reply_content(
    text: &str,
    parts: &[leviath_core::mime::Part],
    sink: Option<&crate::context_setup::PartSink<'_>>,
) -> Option<leviath_core::region::EntryContent> {
    let mut all = Vec::with_capacity(parts.len() + 1);
    if !text.trim().is_empty() {
        all.push(crate::context_setup::text_part(sink, "reply.txt", text));
    }
    all.extend(parts.iter().cloned());
    (!all.is_empty()).then(|| leviath_core::region::EntryContent::from_parts(all))
}
