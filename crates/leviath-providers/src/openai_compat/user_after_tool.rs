//! A user turn directly after a tool result, for the models whose API refuses
//! that order.

/// Whether this model's API refuses a user turn directly after a tool result.
///
/// Mistral's does: `Unexpected role 'user' after role 'tool'`. Matched on the
/// model rather than the provider, since the same models are reached through
/// OpenRouter, Ollama and compatible gateways.
fn refuses_user_after_tool(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    [
        "mistral",
        "codestral",
        "devstral",
        "magistral",
        "ministral",
        "pixtral",
        "voxtral",
    ]
    .iter()
    .any(|family| model.contains(family))
}

/// For a model that refuses a user turn right after a tool result, fold that
/// turn's text into the tool result before it.
///
/// The runtime sends what a region gained during a stage as a user turn after
/// the newest tool results (see `leviath_runtime::components::PromptFrame`), and a
/// message delivered while tools ran lands in the same place. Every other API
/// takes that order.
///
/// Either side may already be text parts rather than a string: a route that
/// marks turns for the cache writes a marked turn as one text part carrying
/// the marker. Strings join as a string; otherwise both become text parts and
/// the turn's parts follow the result's, markers and all, so the marker stays
/// at the end of what it covered. A turn holding anything but text (stored
/// parts) is left where it is, and so is one after a tool result whose content
/// is neither, so nothing is ever folded into a result it would replace.
pub(crate) fn fold_user_after_tool(
    messages: Vec<serde_json::Value>,
    model: &str,
) -> Vec<serde_json::Value> {
    if !refuses_user_after_tool(model) {
        return messages;
    }
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(messages.len());
    for message in messages {
        let folded = match out.last_mut() {
            Some(last) if last["role"] == "tool" && message["role"] == "user" => {
                fold(&mut last["content"], &message["content"])
            }
            _ => false,
        };
        if !folded {
            out.push(message);
        }
    }
    out
}

/// Join `turn` onto the end of `result`, or leave both alone and say so.
fn fold(result: &mut serde_json::Value, turn: &serde_json::Value) -> bool {
    if let (Some(said), Some(text)) = (result.as_str(), turn.as_str()) {
        *result = serde_json::Value::String(format!("{said}\n\n{text}"));
        return true;
    }
    let (Some(mut joined), Some(mut parts)) = (text_parts(result), text_parts(turn)) else {
        return false;
    };
    // Nothing to join onto, or nothing to join: no separator.
    if let Some(first) = parts.first_mut().filter(|_| !joined.is_empty()) {
        let text = first["text"].as_str().unwrap_or_default();
        first["text"] = serde_json::Value::String(format!("\n\n{text}"));
    }
    joined.append(&mut parts);
    *result = serde_json::Value::Array(joined);
    true
}

/// `content` as text parts: a string as one, an array when every element is
/// a text part, else `None`.
fn text_parts(content: &serde_json::Value) -> Option<Vec<serde_json::Value>> {
    match content {
        serde_json::Value::String(text) => {
            Some(vec![serde_json::json!({ "type": "text", "text": text })])
        }
        serde_json::Value::Array(parts) if parts.iter().all(|p| p["type"] == "text") => {
            Some(parts.clone())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MISTRAL: &str = "mistralai/devstral-2";

    fn tool(content: serde_json::Value) -> serde_json::Value {
        json!({ "role": "tool", "tool_call_id": "c1", "content": content })
    }

    fn user(content: serde_json::Value) -> serde_json::Value {
        json!({ "role": "user", "content": content })
    }

    fn marked(text: &str) -> serde_json::Value {
        json!([{ "type": "text", "text": text, "cache_control": { "type": "ephemeral" } }])
    }

    /// A marked turn, as a route that marks the newest message writes it,
    /// still folds, and its marker ends the result it joined.
    #[test]
    fn a_marked_turn_folds_with_its_marker_last() {
        let out = fold_user_after_tool(vec![tool(json!("r")), user(marked("note"))], MISTRAL);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0]["content"],
            json!([
                { "type": "text", "text": "r" },
                { "type": "text", "text": "\n\nnote", "cache_control": { "type": "ephemeral" } },
            ])
        );
    }

    /// A marked result keeps its text when a plain turn joins it.
    #[test]
    fn a_marked_result_keeps_its_text() {
        let out = fold_user_after_tool(vec![tool(marked("r")), user(json!("note"))], MISTRAL);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["content"][0]["text"], "r");
        assert_eq!(out[0]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(out[0]["content"][1]["text"], "\n\nnote");
    }

    /// An empty result takes the turn with no separator; a result that is not
    /// text, or a turn that is not, stays where it is.
    #[test]
    fn only_text_folds_and_never_replaces_a_result() {
        let out = fold_user_after_tool(vec![tool(json!([])), user(json!("note"))], MISTRAL);
        assert_eq!(
            out[0]["content"],
            json!([{ "type": "text", "text": "note" }])
        );
        let kept = vec![tool(serde_json::Value::Null), user(json!("note"))];
        assert_eq!(fold_user_after_tool(kept.clone(), MISTRAL), kept);
        let picture = vec![
            tool(json!("r")),
            user(json!([{ "type": "image_url", "image_url": { "url": "data:," } }])),
        ];
        assert_eq!(fold_user_after_tool(picture.clone(), MISTRAL), picture);
    }
}
