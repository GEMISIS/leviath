//! OpenRouter's `reasoning_details`, kept so a later turn can send them back.
//!
//! A reasoning model reached through OpenRouter returns its thinking as an
//! array of typed blocks (`reasoning.text`, `reasoning.summary`,
//! `reasoning.encrypted`). Sent back unmodified on the assistant message that
//! produced them, they keep the model's reasoning across a tool call; Gemini
//! reached this way issues its thought signature as an encrypted block, and
//! Claude its signed thinking. They are one model's, so the blob that carries
//! them names the provider and the model that ANSWERED (the `model` the reply
//! reports, not the one asked for, which may be a router):
//!
//! ```json
//! {"openrouter": {"provider": "openrouter", "model": "google/gemini-4-argon",
//!                 "details": [{"type": "reasoning.encrypted", "data": "..."}]}}
//! ```
//!
//! # The replay rule
//!
//! The blocks are sent back only when the request names a concrete model whose
//! id equals the answering model's, ignoring a `:variant` suffix (`:nitro`,
//! `:floor` choose a route to the same model). A request that lets the gateway
//! pick (`openrouter/auto`, a `models` list in `extra`) may land on a different
//! model than the one that signed the blocks, so it replays none. Another
//! protocol's blob (`{"responses": ...}`, `{"bedrock": ...}`) never opens here.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

/// The key a sealed blob's object sits under.
const KEY: &str = "openrouter";

/// The registry name a blob is sealed under.
const PROVIDER: &str = "openrouter";

/// The fields whose fragments join as a stream delivers them.
const FRAGMENTS: [&str; 3] = ["text", "summary", "data"];

/// The `reasoning_details` a streamed answer delivered, merged.
///
/// A stream splits one block over many chunks, each naming the block by its
/// `index`. Per field: a text, summary or data fragment joins what came before
/// it; any other field (`id`, `format`, `signature`) takes the last non-empty
/// value; a `null`, or an empty string over a held value, never overwrites what
/// is held (Claude ends a block with `"text": null` and a `signature`). A block
/// continues another of the same `type` and `index` unless both name an `id`
/// and the ids differ, which starts a new one.
#[derive(Default)]
pub(crate) struct Details {
    /// Whether this stream keeps anything. A route that does not replay its
    /// reasoning reads the stream as before.
    keeping: bool,
    /// The model the first chunk said answered.
    answered: Option<String>,
    /// The merged blocks, in first-seen order.
    blocks: Vec<Value>,
    /// Set once the blob has gone out: it is sent once, at the end.
    sealed: bool,
}

/// One stream's [`Details`], shared by the framer and the end-of-bytes flush.
pub(crate) type SharedDetails = Arc<Mutex<Details>>;

/// A stream's reasoning blocks, kept.
#[cfg(feature = "openrouter")]
pub(crate) fn keeping() -> SharedDetails {
    Arc::new(Mutex::new(Details::keeping()))
}

/// A stream that keeps nothing: the reasoning text still shows, but no blocks
/// are held for replay.
pub(crate) fn discarding() -> SharedDetails {
    Arc::new(Mutex::new(Details::default()))
}

impl Details {
    /// A stream whose reasoning is kept.
    #[cfg(feature = "openrouter")]
    pub(crate) fn keeping() -> Self {
        Self {
            keeping: true,
            ..Self::default()
        }
    }

    /// Take in what a streamed chunk carries: the model that answered, and the
    /// blocks in its delta.
    pub(crate) fn absorb(&mut self, chunk: &Value, delta: &Value) {
        if !self.keeping {
            return;
        }
        if self.answered.is_none() {
            self.answered = chunk
                .get("model")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .map(str::to_string);
        }
        let incoming = delta.get("reasoning_details").and_then(Value::as_array);
        for block in incoming.into_iter().flatten() {
            self.merge(block);
        }
    }

    /// Fold one block into the one it continues, or keep it as a new one.
    fn merge(&mut self, block: &Value) {
        let Some(fields) = block.as_object() else {
            return;
        };
        let continues = |kept: &&mut Value| {
            let held = kept.get("id").and_then(Value::as_str);
            let new = fields.get("id").and_then(Value::as_str);
            fields.get("index").is_some()
                && kept.get("index") == fields.get("index")
                && kept.get("type") == fields.get("type")
                && !matches!((held, new), (Some(held), Some(new)) if held != new)
        };
        let Some(kept) = self.blocks.iter_mut().find(continues) else {
            self.blocks.push(block.clone());
            return;
        };
        for (key, value) in fields {
            if value.is_null() {
                continue;
            }
            let held = kept.get(key).filter(|v| !v.is_null());
            let merged = if FRAGMENTS.contains(&key.as_str()) {
                match (held.and_then(Value::as_str), value.as_str()) {
                    (Some(held), Some(more)) => json!(format!("{held}{more}")),
                    (Some(_), None) => continue,
                    (None, _) => value.clone(),
                }
            } else if held.is_some() && value.as_str() == Some("") {
                continue;
            } else {
                value.clone()
            };
            kept[key] = merged;
        }
    }

    /// The sealed blob for everything taken in, exactly once, for the end of
    /// the stream; `model` stands in when no chunk said which model answered.
    pub(crate) fn finish(&mut self, model: &str) -> Option<String> {
        if !self.keeping || std::mem::replace(&mut self.sealed, true) {
            return None;
        }
        let blocks = std::mem::take(&mut self.blocks);
        seal(self.answered.as_deref().unwrap_or(model), &blocks)
    }

    /// Whether a keeping stream has sent its blob, which is the end of it.
    pub(crate) fn is_sealed(&self) -> bool {
        self.keeping && self.sealed
    }
}

/// Seal `blocks` for the model that produced them; `None` when there are none.
pub(crate) fn seal(model: &str, blocks: &[Value]) -> Option<String> {
    (!blocks.is_empty()).then(|| {
        json!({ KEY: { "provider": PROVIDER, "model": model, "details": blocks } }).to_string()
    })
}

/// A model id without its `:variant` suffix.
#[cfg(feature = "openrouter")]
fn concrete(model: &str) -> &str {
    model.split(':').next().unwrap_or(model)
}

/// The blocks to send back on an assistant message: those `blob` holds when
/// OpenRouter sealed it, the model that answered is the model `asked` for, and
/// the request does not leave the choice of model to the gateway (`routed`).
/// See the module docs.
#[cfg(feature = "openrouter")]
pub(crate) fn replay(asked: &str, routed: bool, blob: &str) -> Option<Vec<Value>> {
    let value: Value = serde_json::from_str(blob).ok()?;
    let sealed = value.get(KEY)?;
    let answered = sealed.get("model").and_then(Value::as_str)?;
    let same = sealed.get("provider").and_then(Value::as_str) == Some(PROVIDER)
        && !routed
        && concrete(answered) == concrete(asked);
    let blocks = sealed.get("details")?.as_array()?;
    (same && !blocks.is_empty()).then(|| blocks.clone())
}

/// The blob for a buffered response's `message`, sealed to the model the body
/// says answered (`asked` when it names none).
#[cfg(feature = "openrouter")]
pub(crate) fn seal_message(asked: &str, body: &Value, message: &Value) -> Option<String> {
    let blocks = message.get("reasoning_details")?.as_array()?;
    let answered = body.get("model").and_then(Value::as_str).unwrap_or(asked);
    seal(answered, blocks)
}

#[cfg(test)]
#[path = "details_tests.rs"]
mod tests;
