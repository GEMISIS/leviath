//! What a message carries: text, tool calls, tool results and mime parts.
//!
//! Split out of `provider.rs` for the 1200-line rule, not as an interface
//! change: every type here is re-exported from `crate::provider`.

use serde::{Deserialize, Serialize};

/// Rich message content: either a plain text string or structured content blocks.
///
/// Provider serialization converts this to the appropriate API format
/// (e.g., Anthropic content blocks, OpenAI message + tool_calls).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// Plain text content (backward compatible).
    Text(String),
    /// Structured content blocks (tool_use, tool_result, text).
    Blocks(Vec<ContentBlock>),
}

impl From<String> for MessageContent {
    fn from(s: String) -> Self {
        MessageContent::Text(s)
    }
}

impl From<&str> for MessageContent {
    fn from(s: &str) -> Self {
        MessageContent::Text(s.to_string())
    }
}

impl From<leviath_core::region::EntryContent> for MessageContent {
    /// The text an entry reads as. Assembly turns an entry's stored parts
    /// into mime blocks itself; this is the plain-text path.
    fn from(c: leviath_core::region::EntryContent) -> Self {
        MessageContent::Text(c.into_string())
    }
}

impl MessageContent {
    /// Get the plain text content, concatenating text blocks if needed.
    pub fn as_text(&self) -> String {
        match self {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

/// A content block within a rich message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// A text content block.
    #[serde(rename = "text")]
    Text {
        /// The text itself.
        text: String,
    },
    /// A tool use request from the assistant.
    #[serde(rename = "tool_use")]
    ToolUse {
        /// Provider-assigned call id, which the matching result must quote back.
        id: String,
        /// The tool the model asked for.
        name: String,
        /// Arguments as the model supplied them, before any validation.
        input: serde_json::Value,
        /// See [`super::ToolCall::thought_signature`]: replayed verbatim so a
        /// provider that requires it accepts the follow-up request.
        ///
        /// **Never serialized.** This is one provider's field riding in shared
        /// history, and history is replayed to whichever provider runs next -
        /// which, with per-stage models, is routinely a different one. Anthropic
        /// rejects the unknown key outright (`tool_use.thought_signature: Extra
        /// inputs are not permitted`), so a Gemini stage followed by an
        /// Anthropic stage dies on its first request.
        ///
        /// A provider that wants it emits it deliberately rather than getting
        /// it by default: `openai_compat` already does exactly that when
        /// building its tool calls, which is why the OpenAI-shaped path
        /// (Gemini included) keeps working. The field stays on the struct - it
        /// is still needed in memory and still persisted through
        /// `SerializedToolCall` - it just never reaches a body nobody asked to
        /// put it in.
        #[serde(default, skip_serializing)]
        thought_signature: Option<String>,
    },
    /// A tool result from executing a tool.
    #[serde(rename = "tool_result")]
    ToolResult {
        /// The [`ContentBlock::ToolUse`] id this answers.
        tool_use_id: String,
        /// The tool's output as text. Every provider takes a string here, so a
        /// structured result is already rendered by this point.
        content: String,
        /// Whether the tool refused or failed.
        is_error: bool,
    },
    /// A stored mime part: an image, a clip, a document, anything that is
    /// not text.
    ///
    /// The neutral form. Assembly emits one per stored part with `data` empty;
    /// hydration (`crate::mime::hydrate_request`) fills `data` with the base64
    /// bytes when the model takes the type, or turns the block into text. A
    /// part the vendor already holds carries `remote` instead of bytes. A
    /// built-in provider encodes a hydrated block into its own shape (an
    /// Anthropic `image` block, an OpenAI `image_url` part); a block that
    /// reaches a provider with `data` still empty is sent as its stand-in
    /// text, so no lane has to hydrate to stay correct. A Rhai provider sees
    /// this form as it is.
    #[serde(rename = "mime")]
    Mime {
        /// What the part is: hash, type, size, dimensions, the stand-in text.
        part: leviath_core::mime::BlobRef,
        /// The bytes, base64, once hydrated. Empty until then.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        data: String,
        /// The part's name, when it has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// The part's delivery override, when it has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deliver: Option<leviath_core::mime::Delivery>,
        /// The vendor's copy of the part, when it was uploaded: the request
        /// names it by id and carries no bytes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote: Option<crate::files::RemoteFile>,
    },
}

impl ContentBlock {
    /// A mime block for `part`, unhydrated.
    pub fn mime(part: &leviath_core::mime::Part) -> Option<Self> {
        let blob = part.blob()?;
        Some(ContentBlock::Mime {
            part: blob.clone(),
            data: String::new(),
            name: part.name.clone(),
            deliver: part.deliver,
            remote: None,
        })
    }

    /// The stand-in text of a mime block, or `None` for any other block.
    pub fn stand_in(&self) -> Option<&str> {
        match self {
            ContentBlock::Mime { part, .. } => Some(part.stand_in.as_str()),
            _ => None,
        }
    }

    /// Whether this is a mime block carrying its bytes, or naming the
    /// vendor's copy of them.
    pub fn is_hydrated_mime(&self) -> bool {
        matches!(self, ContentBlock::Mime { data, remote, .. } if !data.is_empty() || remote.is_some())
    }
}

/// A system prompt block, separated from conversation messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemBlock {
    /// The text content of this system block.
    pub text: String,
    /// Cache hint for this system block.
    pub cache_hint: leviath_core::CacheHint,
    /// The region this block was rendered from, for diagnostics.
    ///
    /// Empty for a block that is not a region - a hint, a tool preamble - which
    /// is exactly the set of blocks no volatility warning could be about.
    ///
    /// Defaulted on the wire so a request serialized before these fields
    /// existed still deserializes - a script provider that round-trips one, or
    /// a dumped body replayed later, must not fail on a field it predates.
    #[serde(default)]
    pub region: String,
    /// How much the region this block came from moves between requests.
    ///
    /// Carried so assembly can order blocks by it: a provider caches by prefix,
    /// so a block that moves invalidates everything behind it, and the
    /// arrangement that pays is stable content first and churn last. The
    /// region's *kind* cannot answer this - a pinned region is written
    /// constantly - so the blueprint says and this carries the answer.
    #[serde(default)]
    pub volatility: leviath_core::Volatility,
}

/// An `f32` as JSON, at the precision it was written with.
///
/// `serde_json` widens an `f32` to `f64` to store it, and `0.7f32` widened is
/// `0.699999988079071`. That is what every request carried: it read as a
/// Leviath bug in provider error messages, and Z.AI rejects it outright with
/// `The temperature parameter is illegal: 限制小数点[2]位` - at most two decimal
/// places - which made an entire vendor family unusable.
///
/// `f32`'s own `Display` gives the shortest decimal that round-trips back to
/// the same `f32`, so `0.7f32` prints "0.7". Parsing that as `f64` gets the
/// number the blueprint author actually wrote, without imposing a fixed
/// precision on someone who wanted `0.125`.
pub(crate) fn json_number(value: f32) -> serde_json::Value {
    // `f32::Display` always produces a decimal that parses back, including for
    // the non-finite values, so the fallback is the same number rather than a
    // branch nothing reaches.
    serde_json::json!(value.to_string().parse::<f64>().unwrap_or(f64::from(value)))
}
