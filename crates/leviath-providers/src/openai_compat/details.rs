//! OpenRouter's `reasoning_details`, kept so a later turn can send them back.
//!
//! A reasoning model reached through OpenRouter returns its thinking as an
//! array of typed blocks (`reasoning.text`, `reasoning.summary`,
//! `reasoning.encrypted`). Sent back unmodified on the assistant message that
//! produced them, they keep the model's reasoning across a tool call; Gemini
//! reached this way issues its thought signature as an encrypted block, and
//! Claude its signed thinking. They are one model's, so the blob that carries
//! them names the provider and the model:
//!
//! ```json
//! {"openrouter": {"provider": "openrouter", "model": "google/gemini-4-argon",
//!                 "details": [{"type": "reasoning.encrypted", "data": "..."}]}}
//! ```
//!
//! and opens only for that provider and that model. Another protocol's blob
//! (`{"responses": ...}`, `{"bedrock": ...}`) never opens here.

use serde_json::{Value, json};

/// The key a sealed blob's object sits under.
const KEY: &str = "openrouter";

/// The registry name a blob is sealed under.
const PROVIDER: &str = "openrouter";

/// The fields whose fragments join as a stream delivers them; every other
/// field (`id`, `format`, `signature`, `index`) is the last one seen.
const FRAGMENTS: [&str; 3] = ["text", "summary", "data"];

/// The `reasoning_details` a streamed answer delivered, merged.
///
/// A stream splits one block over many chunks, each naming the block by its
/// `index`; the text, summary or data fragments of one block are joined in the
/// order they came, and blocks keep the order they were first seen in.
#[derive(Default)]
pub(crate) struct Details {
    /// The model the blocks are sealed to. `None` keeps nothing: a route that
    /// does not replay them reads the stream as before.
    model: Option<String>,
    /// The merged blocks, in first-seen order.
    blocks: Vec<Value>,
}

impl Details {
    /// Keep the blocks of a stream `model` is answering.
    pub(crate) fn for_model(model: &str) -> Self {
        Self {
            model: Some(model.to_string()),
            blocks: Vec::new(),
        }
    }

    /// Take in the blocks a streamed `delta` carries.
    pub(crate) fn absorb(&mut self, delta: &Value) {
        if self.model.is_none() {
            return;
        }
        let incoming = delta.get("reasoning_details").and_then(Value::as_array);
        for block in incoming.into_iter().flatten() {
            self.merge(block);
        }
    }

    /// Fold one block into the one it continues, or keep it as a new one. A
    /// block continues another of the same `type` and `index`; one with no
    /// `index` is never a continuation.
    fn merge(&mut self, block: &Value) {
        let Some(fields) = block.as_object() else {
            return;
        };
        let continues = |kept: &&mut Value| {
            fields.get("index").is_some()
                && kept.get("index") == fields.get("index")
                && kept.get("type") == fields.get("type")
        };
        let Some(kept) = self.blocks.iter_mut().find(continues) else {
            self.blocks.push(block.clone());
            return;
        };
        for (key, value) in fields {
            let joined = FRAGMENTS
                .contains(&key.as_str())
                .then(|| kept.get(key).and_then(Value::as_str).zip(value.as_str()))
                .flatten()
                .map(|(held, more)| Value::String(format!("{held}{more}")));
            kept[key] = joined.unwrap_or_else(|| value.clone());
        }
    }

    /// The sealed blob for what has been taken in, once; `None` when nothing
    /// was, or when this stream keeps none.
    pub(crate) fn seal(&mut self) -> Option<String> {
        let model = self.model.as_deref()?;
        let blocks = std::mem::take(&mut self.blocks);
        seal(model, &blocks)
    }
}

/// Seal `blocks` for the model that produced them; `None` when there are none.
pub(crate) fn seal(model: &str, blocks: &[Value]) -> Option<String> {
    (!blocks.is_empty()).then(|| {
        json!({ KEY: { "provider": PROVIDER, "model": model, "details": blocks } }).to_string()
    })
}

/// The blocks to send back on an assistant message: those `blob` holds when
/// OpenRouter sealed it for `model`, none otherwise.
pub(crate) fn replay(model: &str, blob: &str) -> Option<Vec<Value>> {
    let value: Value = serde_json::from_str(blob).ok()?;
    let sealed = value.get(KEY)?;
    let same = sealed.get("provider").and_then(Value::as_str) == Some(PROVIDER)
        && sealed.get("model").and_then(Value::as_str) == Some(model);
    let blocks = sealed.get("details")?.as_array()?;
    (same && !blocks.is_empty()).then(|| blocks.clone())
}

/// The blob for a buffered response's `message`.
pub(crate) fn seal_message(model: &str, message: &Value) -> Option<String> {
    let blocks = message.get("reasoning_details")?.as_array()?;
    seal(model, blocks)
}

#[cfg(test)]
#[path = "details_tests.rs"]
mod tests;
