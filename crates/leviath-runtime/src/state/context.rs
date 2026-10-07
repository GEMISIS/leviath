//! A run's context as the run file stores it: every region and every entry,
//! typed all the way down.

use leviath_core::JsonDoc;
use leviath_core::mime::Delivery;
use leviath_core::taint::TaintLevel;
use serde::{Deserialize, Serialize};

use crate::spec::names::{Digest, RegionName};

/// The whole context window.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContextState {
    /// The regions, in the order the model reads them.
    pub regions: Vec<RegionState>,
    /// Regions the current stage does not show the model.
    pub hidden: Vec<RegionName>,
    /// The window's token budget.
    pub max_tokens: u32,
}

impl ContextState {
    /// A region by name.
    pub fn region(&self, name: &str) -> Option<&RegionState> {
        self.regions.iter().find(|r| r.name.as_str() == name)
    }
}

/// One region's live state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RegionState {
    /// The region.
    pub name: RegionName,
    /// Its budget for the current stage, in tokens.
    pub max_tokens: u32,
    /// The tokens its entries take now.
    pub current_tokens: u32,
    /// Whether its messages are due to be compacted.
    pub needs_message_compaction: bool,
    /// How sensitive its content is, when taint tracking is on.
    pub taint: Option<TaintState>,
    /// Its entries, oldest first.
    pub entries: Vec<EntryState>,
}

/// How sensitive a region's content is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TaintState {
    /// The highest level of anything in it.
    pub level: TaintLevel,
    /// Each entry's level, in entry order.
    pub entries: Vec<TaintLevel>,
}

/// One entry in a region.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EntryState {
    /// Its text.
    pub text: String,
    /// Its typed parts (images, files), beside the text.
    pub parts: Vec<PartState>,
    /// The tokens it takes.
    pub tokens: u32,
    /// When it was written, in unix seconds.
    pub timestamp: i64,
    /// What kind of message it is.
    pub kind: EntryKind,
    /// What the engine keeps about it besides its content.
    pub meta: EntryMeta,
    /// Its key, in a keyed region.
    pub key: Option<String>,
    /// The model's reasoning that came with it, when the provider returned any.
    pub reasoning: Option<String>,
}

/// What kind of message an entry is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum EntryKind {
    /// Plain text.
    Text,
    /// Something a person said.
    UserMessage,
    /// A model turn, with the tool calls it made.
    AssistantTurn(Vec<ToolCallState>),
    /// A tool's result.
    ToolResult {
        /// The call it answers.
        call_id: String,
        /// The tool, as the model named it.
        tool: String,
        /// Whether the tool failed.
        is_error: bool,
    },
}

/// A tool call as a model wrote it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolCallState {
    /// The provider's id for the call.
    pub id: String,
    /// The tool, as the model named it (it may name one that does not exist).
    pub name: String,
    /// The arguments.
    pub args: JsonDoc,
    /// The provider's signature over the model's thinking, to send back.
    pub thought_signature: Option<String>,
}

/// What the engine keeps about an entry besides its content.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum EntryMeta {
    /// Nothing.
    #[default]
    None,
    /// A checklist item.
    ChecklistItem {
        /// The item's number.
        id: u32,
        /// Whether it is done.
        done: bool,
        /// The note left when it was ticked.
        note: Option<String>,
    },
}

/// A typed part of an entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PartState {
    /// Its mime type.
    pub mime_type: String,
    /// The part's content.
    pub body: PartBody,
    /// Its file name, when it had one.
    pub name: Option<String>,
    /// How a model receives it, over the default.
    pub deliver: Option<Delivery>,
}

/// Where a part's content is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum PartBody {
    /// Small text, held in place.
    Inline(String),
    /// Bytes in the run file's blob frames.
    Stored(BlobState),
}

/// A stored part's bytes, by digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BlobState {
    /// The bytes' sha256.
    pub digest: Digest,
    /// How many bytes.
    pub size: u64,
    /// Width in pixels, for an image or video.
    pub width: Option<u32>,
    /// Height in pixels, for an image or video.
    pub height: Option<u32>,
    /// Length, for audio or video.
    pub duration_ms: Option<u64>,
    /// The tokens it takes.
    pub tokens: u32,
    /// What a model that cannot take the type sees instead.
    pub stand_in: String,
}

/// How one region changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum RegionChange {
    /// Entries were added at the end and nothing else changed in them.
    Append(Vec<EntryState>),
    /// The entries were rewritten.
    Replace(Vec<EntryState>),
}

/// How the context changed between two states.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContextDiff {
    /// Region changes, by region.
    pub regions: Vec<(RegionName, RegionHead, Option<RegionChange>)>,
    /// Regions that went away.
    pub removed: Vec<RegionName>,
    /// The region order, when it changed.
    pub order: Option<Vec<RegionName>>,
    /// The hidden list, when it changed.
    pub hidden: Option<Vec<RegionName>>,
    /// The window budget, when it changed.
    pub max_tokens: Option<u32>,
}

/// A region's fields besides its entries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RegionHead {
    /// Its budget.
    pub max_tokens: u32,
    /// Its tokens now.
    pub current_tokens: u32,
    /// Whether its messages are due for compaction.
    pub needs_message_compaction: bool,
    /// Its taint.
    pub taint: Option<TaintState>,
}

impl RegionState {
    fn head(&self) -> RegionHead {
        RegionHead {
            max_tokens: self.max_tokens,
            current_tokens: self.current_tokens,
            needs_message_compaction: self.needs_message_compaction,
            taint: self.taint.clone(),
        }
    }
}

impl ContextDiff {
    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
            && self.removed.is_empty()
            && self.order.is_none()
            && self.hidden.is_none()
            && self.max_tokens.is_none()
    }

    /// What changed from `prev` to `next`. A region whose entries only grew
    /// records just the new ones, which is the common case between turns.
    pub fn between(prev: &ContextState, next: &ContextState) -> Self {
        let mut regions = Vec::new();
        for r in &next.regions {
            let before = prev.region(r.name.as_str());
            let entries = match before {
                Some(b) if b.entries == r.entries => None,
                Some(b) if r.entries.starts_with(&b.entries) => {
                    Some(RegionChange::Append(r.entries[b.entries.len()..].to_vec()))
                }
                _ => Some(RegionChange::Replace(r.entries.clone())),
            };
            let head_changed = before.is_none_or(|b| b.head() != r.head());
            if entries.is_some() || head_changed {
                regions.push((r.name.clone(), r.head(), entries));
            }
        }
        let removed = prev
            .regions
            .iter()
            .filter(|p| next.region(p.name.as_str()).is_none())
            .map(|p| p.name.clone())
            .collect();
        let names = |s: &ContextState| s.regions.iter().map(|r| r.name.clone()).collect::<Vec<_>>();
        let order = names(next);
        Self {
            regions,
            removed,
            order: (names(prev) != order).then_some(order),
            hidden: (prev.hidden != next.hidden).then(|| next.hidden.clone()),
            max_tokens: (prev.max_tokens != next.max_tokens).then_some(next.max_tokens),
        }
    }

    /// Apply this diff to the state it was taken from.
    pub fn apply(&self, state: &mut ContextState) {
        state.regions.retain(|r| !self.removed.contains(&r.name));
        for (name, head, change) in &self.regions {
            let pos = state.regions.iter().position(|r| &r.name == name);
            let mut region = match pos {
                Some(i) => state.regions.remove(i),
                None => RegionState {
                    name: name.clone(),
                    max_tokens: 0,
                    current_tokens: 0,
                    needs_message_compaction: false,
                    taint: None,
                    entries: Vec::new(),
                },
            };
            region.max_tokens = head.max_tokens;
            region.current_tokens = head.current_tokens;
            region.needs_message_compaction = head.needs_message_compaction;
            region.taint = head.taint.clone();
            match change {
                Some(RegionChange::Append(more)) => region.entries.extend(more.iter().cloned()),
                Some(RegionChange::Replace(all)) => region.entries = all.clone(),
                None => {}
            }
            let at = pos.unwrap_or(state.regions.len());
            state.regions.insert(at, region);
        }
        if let Some(order) = &self.order {
            state
                .regions
                .sort_by_key(|r| order.iter().position(|n| n == &r.name));
        }
        if let Some(hidden) = &self.hidden {
            state.hidden = hidden.clone();
        }
        if let Some(max) = self.max_tokens {
            state.max_tokens = max;
        }
    }
}
