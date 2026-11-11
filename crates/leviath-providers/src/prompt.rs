//! The prompt of a model that is not a chat model.
//!
//! A model that answers in images, audio or video and never in text takes
//! one prompt, not a conversation: it has no system prompt to read, and the
//! runtime's opening turn ([`OPENING_TURN`](crate::provider::OPENING_TURN))
//! means nothing to it. Shown "Begin." with the stage's instruction in a
//! system message, such a model draws the word "Begin." (an OpenRouter image
//! model did exactly that, once per tile, for a whole sheet).
//!
//! [`request_text`] is what that prompt is, on every route: the regions the
//! stage named as its prompt, or else the conversation's own text, or else
//! the text its regions hold. The routes that call a media endpoint read it
//! there; [`prompt_only`] hands it to a route that speaks chat, as the one
//! user turn; and [`unprompted`] says when a request holds nothing to draw
//! from at all, so it can be refused instead of sent.

use crate::provider::{ContentBlock, InferenceRequest, Message, MessageContent};

/// `request` as a model that takes one prompt reads it: no system blocks, and
/// one user turn holding the media parts the request carried, then
/// [`request_text`].
///
/// The stage's named prompt regions are spent in that text, so the key that
/// names them is dropped, and a route reading the result reads the same
/// prompt the request had. A request with no prompt text is handed back as
/// it came, for [`unprompted`] to judge.
pub fn prompt_only(mut request: InferenceRequest) -> InferenceRequest {
    let prompt = request_text(&request);
    if prompt.is_empty() {
        return request;
    }
    let mut blocks: Vec<ContentBlock> = request
        .messages
        .iter()
        .filter_map(|message| match &message.content {
            MessageContent::Blocks(blocks) => Some(blocks),
            MessageContent::Text(_) => None,
        })
        .flatten()
        .filter(|block| matches!(block, ContentBlock::Mime { .. }))
        .cloned()
        .collect();
    let content = match blocks.is_empty() {
        true => MessageContent::Text(prompt),
        false => {
            blocks.push(ContentBlock::Text { text: prompt });
            MessageContent::Blocks(blocks)
        }
    };
    request.system.clear();
    request.messages = vec![Message {
        role: "user".to_string(),
        content,
        cache_breakpoint: false,
        reasoning: None,
    }];
    if let Some(extra) = request.extra.as_object_mut() {
        extra.remove(crate::provider::PROMPT_REGIONS_KEY);
    }
    request
}

/// Whether `request` holds nothing a model that takes one prompt could work
/// from: no prompt text and no media part, only the runtime's opening turn
/// and the blocks it never reads.
pub fn unprompted(request: &InferenceRequest) -> bool {
    let carries_media = request
        .messages
        .iter()
        .any(|message| match &message.content {
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Mime { .. })),
            MessageContent::Text(_) => false,
        });
    !carries_media && request_text(request).is_empty()
}

/// The plain text of a request, across its text blocks and plain messages.
///
/// The texture prompt an upstream stage wrote lands here, as the text of the
/// region it wrote it to; it is the dynamic hint the operation textures with.
pub fn request_text(request: &InferenceRequest) -> String {
    if let Some(names) = prompt_regions(request) {
        return regions_text(request, &names);
    }
    // Assembly emits a stored part as a pointer text block ("[region] [mime,
    // size] name") beside its bytes block. That pointer is not the user's
    // prompt, so exclude any text carrying a stored part's stand-in, or the
    // texture prompt and the animation action would be the mesh's file line
    // rather than "weathered bronze" or "walk".
    let stand_ins: Vec<&str> = request
        .messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::Blocks(blocks) => Some(blocks),
            MessageContent::Text(_) => None,
        })
        .flatten()
        .filter_map(|b| match b {
            ContentBlock::Mime { part, .. } => Some(part.stand_in.as_str()),
            _ => None,
        })
        .filter(|s| !s.is_empty())
        .collect();
    // The runtime's placeholder turn is not a prompt either: a stage whose
    // prompt lives in a pinned region has no user turn but this one, and
    // reading it sent the model "Begin." while the task sat unread below.
    let is_pointer = |text: &str| {
        text.trim() == crate::provider::OPENING_TURN || stand_ins.iter().any(|s| text.contains(s))
    };

    let mut chunks = Vec::new();
    for message in &request.messages {
        match &message.content {
            MessageContent::Text(text) => {
                if !is_pointer(text) {
                    chunks.push(text.clone());
                }
            }
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    if let ContentBlock::Text { text } = block
                        && !is_pointer(text)
                    {
                        chunks.push(text.clone());
                    }
                }
            }
        }
    }
    // A pinned region renders into the system prompt, not a message, so a
    // task or prompt that lives in one never reached here and every such
    // stage ran on the default action or refused for want of a prompt. With
    // no message text, read the system blocks instead: only the ones a region
    // produced (a hint carries no region), never the runtime's own
    // instruction blocks, and never a region whose text is a stored part's
    // stand-in. Message text still wins when there is any, so a prompt an
    // upstream stage wrote into a conversation is not diluted by the task.
    if chunks.is_empty() {
        for block in &request.system {
            if block.region.is_empty() || RUNTIME_REGIONS.contains(&block.region.as_str()) {
                continue;
            }
            let body = unlabelled(&block.text, &block.region);
            if !is_pointer(body) && !body.trim().is_empty() {
                chunks.push(body.to_string());
            }
        }
    }
    chunks.join("\n").trim().to_string()
}

/// The regions the stage named as its prompt, in order. `None` when it named
/// none, and the prompt is read as it always was.
fn prompt_regions(request: &InferenceRequest) -> Option<Vec<String>> {
    let names: Vec<String> = request
        .extra
        .get(crate::provider::PROMPT_REGIONS_KEY)?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    (!names.is_empty()).then_some(names)
}

/// The text of exactly the named regions, in the order named. Nothing else
/// reaches the model: not the other regions, not the conversation. A region
/// that is hidden or empty contributes nothing, so a prompt that was never
/// written comes back empty rather than as some other region's text.
fn regions_text(request: &InferenceRequest, names: &[String]) -> String {
    let chunks: Vec<&str> = names
        .iter()
        .flat_map(|name| {
            request
                .system
                .iter()
                .filter(move |block| &block.region == name)
                .map(|block| unlabelled(&block.text, &block.region).trim())
        })
        .filter(|body| !body.is_empty())
        .collect();
    chunks.join("\n")
}

/// Regions the runtime writes for its own purposes, whose text is never a
/// prompt: the stage's standing instructions, and the mirrored final answer.
const RUNTIME_REGIONS: &[&str] = &["stage_instructions", "final_output"];

/// A region's system block without the `## <region>` heading assembly puts
/// on it, so the prompt is the region's text and not its label. A block
/// carrying no such heading is returned whole.
fn unlabelled<'a>(text: &'a str, region: &str) -> &'a str {
    text.strip_prefix("## ")
        .and_then(|rest| rest.strip_prefix(region))
        .and_then(|rest| rest.strip_prefix('\n'))
        .unwrap_or(text)
}

#[cfg(test)]
#[path = "prompt_tests.rs"]
mod tests;
