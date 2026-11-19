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
    let stand_ins = stand_ins(request);
    if let Some(names) = prompt_regions(request) {
        return regions_text(request, &names, &stand_ins);
    }
    // Assembly emits a stored part as a pointer text block ("[region] [mime,
    // size] name") beside its bytes block. That pointer is not the user's
    // prompt, so leave out any line that is one, or the texture prompt and
    // the animation action would be the mesh's file line rather than
    // "weathered bronze" or "walk".
    //
    // The runtime's placeholder turn is not a prompt either: a stage whose
    // prompt lives in a pinned region has no user turn but this one, and
    // reading it sent the model "Begin." while the task sat unread below.
    let words = |text: &str| {
        let words = without_pointers(text, &stand_ins);
        let said = text.trim() != crate::provider::OPENING_TURN && !words.trim().is_empty();
        said.then(|| words.into_owned())
    };

    let mut chunks = Vec::new();
    for message in &request.messages {
        match &message.content {
            MessageContent::Text(text) => chunks.extend(words(text)),
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    if let ContentBlock::Text { text } = block {
                        chunks.extend(words(text));
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
    // instruction blocks, and never a stored part's stand-in. Message text
    // still wins when there is any, so a prompt an upstream stage wrote into
    // a conversation is not diluted by the task.
    if chunks.is_empty() {
        for block in &request.system {
            if block.region.is_empty() || RUNTIME_REGIONS.contains(&block.region.as_str()) {
                continue;
            }
            chunks.extend(words(unlabelled(&block.text, &block.region)));
        }
    }
    chunks.join("\n").trim().to_string()
}

/// The stand-in of every stored part `request` carries.
fn stand_ins(request: &InferenceRequest) -> Vec<&str> {
    request
        .messages
        .iter()
        .filter_map(|m| match &m.content {
            MessageContent::Blocks(blocks) => Some(blocks),
            MessageContent::Text(_) => None,
        })
        .flatten()
        .filter_map(ContentBlock::stand_in)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Whether `line` points at a part the request carries and says nothing
/// else: the part's stand-in, alone or behind the `[region]` or
/// `[region / key]` assembly puts before a lifted part.
///
/// A line that only mentions a stand-in among other words is not one. Taking
/// it for one cost a stage its whole prompt: read as one prompt, the request
/// is a single text holding every prompt region, and when one of those
/// regions held a picture the text held its stand-in.
fn is_pointer(line: &str, stand_ins: &[&str]) -> bool {
    let line = line.trim();
    stand_ins.iter().any(|stand_in| {
        line.strip_suffix(stand_in).is_some_and(|before| {
            let before = before.trim_end();
            before.is_empty() || (before.starts_with('[') && before.ends_with(']'))
        })
    })
}

/// `text` without the lines that are [pointers](is_pointer). The part a
/// pointer names goes to the model as the part it is; its file line is not
/// something to draw.
fn without_pointers<'a>(text: &'a str, stand_ins: &[&str]) -> std::borrow::Cow<'a, str> {
    if !text.lines().any(|line| is_pointer(line, stand_ins)) {
        return text.into();
    }
    text.lines()
        .filter(|line| !is_pointer(line, stand_ins))
        .collect::<Vec<_>>()
        .join("\n")
        .into()
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
/// written comes back empty rather than as some other region's text. A
/// stored part a named region holds is in the request as a part, so the line
/// standing in for it is left out of the words.
fn regions_text(request: &InferenceRequest, names: &[String], stand_ins: &[&str]) -> String {
    let chunks: Vec<String> = names
        .iter()
        .flat_map(|name| {
            request
                .system
                .iter()
                .filter(move |block| &block.region == name)
                .map(|block| unlabelled(&block.text, &block.region))
        })
        .map(|body| without_pointers(body, stand_ins).trim().to_string())
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
