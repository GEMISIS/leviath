//! A run's context window as its state holds it, and the change one step made
//! to it.

use async_graphql::{Enum, SimpleObject};
use leviath_core::taint::TaintLevel as CoreTaint;
use leviath_graphql_derive::mirror;
use leviath_runtime::state::context::{
    BlobState, ContextDiff as CoreDiff, ContextState as CoreContext, EntryKind, EntryMeta,
    EntryState, PartBody, PartState, RegionChange, RegionHead, RegionState, TaintState,
    ToolCallState,
};

use super::super::super::mutation::attachments::Delivery;
use super::super::super::scalars::{BigInt, Json, Timestamp};
use super::{big, saturating};

/// How sensitive what a region holds is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum TaintLevel {
    /// Freely shareable.
    Public,
    /// Work-related but not personal.
    Internal,
    /// Personal or highly sensitive.
    Private,
}

impl From<CoreTaint> for TaintLevel {
    fn from(level: CoreTaint) -> Self {
        match level {
            CoreTaint::Public => Self::Public,
            CoreTaint::Internal => Self::Internal,
            CoreTaint::Private => Self::Private,
        }
    }
}

/// A region's taint: its own level and each entry's.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RegionTaint {
    /// The most sensitive level any entry carries.
    pub(crate) level: TaintLevel,
    /// Each entry's level, in entry order.
    pub(crate) entries: Vec<TaintLevel>,
}

impl From<&TaintState> for RegionTaint {
    fn from(taint: &TaintState) -> Self {
        Self {
            level: TaintLevel::from(taint.level),
            entries: taint
                .entries
                .iter()
                .copied()
                .map(TaintLevel::from)
                .collect(),
        }
    }
}

impl From<leviath_core::mime::Delivery> for Delivery {
    fn from(deliver: leviath_core::mime::Delivery) -> Self {
        use leviath_core::mime::Delivery as Core;
        match deliver {
            Core::Native => Self::Native,
            Core::Text => Self::Text,
            Core::StandIn => Self::StandIn,
        }
    }
}

/// A part's bytes, stored once in the run file by digest.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StoredBlob {
    /// The lowercase hex SHA-256 of the bytes.
    pub(crate) digest: String,
    /// Their size.
    pub(crate) size: BigInt,
    /// An image's or a video's width, in pixels.
    pub(crate) width: Option<i32>,
    /// An image's or a video's height, in pixels.
    pub(crate) height: Option<i32>,
    /// A recording's length.
    pub(crate) duration_ms: Option<BigInt>,
    /// What the part costs in a model's window.
    pub(crate) tokens: i32,
    /// The text a model that cannot take the bytes reads instead.
    pub(crate) stand_in: String,
}

impl From<&BlobState> for StoredBlob {
    fn from(b: &BlobState) -> Self {
        Self {
            digest: b.digest.to_string(),
            size: big(b.size),
            width: b.width.map(saturating),
            height: b.height.map(saturating),
            duration_ms: b.duration_ms.map(big),
            tokens: saturating(b.tokens),
            stand_in: b.stand_in.clone(),
        }
    }
}

/// A typed part in a context entry: an image, a document, a recording.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StatePart {
    /// Its mime type.
    pub(crate) mime_type: String,
    /// Its name.
    pub(crate) name: Option<String>,
    /// How it reaches a model, when that was decided for it.
    pub(crate) deliver: Option<Delivery>,
    /// Its text, for a part small enough to keep inline.
    pub(crate) inline: Option<String>,
    /// Its stored bytes, for every other part.
    pub(crate) blob: Option<StoredBlob>,
}

impl From<&PartState> for StatePart {
    fn from(p: &PartState) -> Self {
        let (inline, blob) = match &p.body {
            PartBody::Inline(text) => (Some(text.clone()), None),
            PartBody::Stored(blob) => (None, Some(StoredBlob::from(blob))),
        };
        Self {
            mime_type: p.mime_type.clone(),
            name: p.name.clone(),
            deliver: p.deliver.map(Delivery::from),
            inline,
            blob,
        }
    }
}

/// One tool call a model made.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StateToolCall {
    /// The provider's id for the call.
    pub(crate) call_id: String,
    /// The tool called.
    pub(crate) name: String,
    /// The arguments, as the model wrote them.
    pub(crate) arguments: Json,
    /// The provider's opaque signature of the reasoning behind the call, when
    /// it sent one.
    pub(crate) thought_signature: Option<String>,
}

impl From<&ToolCallState> for StateToolCall {
    fn from(c: &ToolCallState) -> Self {
        Self {
            call_id: c.id.clone(),
            name: c.name.clone(),
            arguments: Json(c.args.value().clone()),
            thought_signature: c.thought_signature.clone(),
        }
    }
}

/// What a context entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum ContextEntryKind {
    /// Plain text.
    Text,
    /// A person's message.
    UserMessage,
    /// A model's turn; its tool calls are under `toolCalls`.
    AssistantTurn,
    /// A tool's result; `callId`, `tool` and `isError` say which.
    ToolResult,
}

/// A checklist item an entry carries.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ChecklistItem {
    /// The item's number in its list.
    pub(crate) number: i32,
    /// Whether it is done.
    pub(crate) done: bool,
    /// A note on it.
    pub(crate) note: Option<String>,
}

/// One entry in a context region.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ContextEntry {
    /// What it is.
    pub(crate) kind: ContextEntryKind,
    /// Its text.
    pub(crate) text: String,
    /// Its typed parts.
    pub(crate) parts: Vec<StatePart>,
    /// What it costs in the window.
    pub(crate) tokens: i32,
    /// When it landed.
    pub(crate) timestamp: Timestamp,
    /// The tool calls an `ASSISTANT_TURN` made.
    pub(crate) tool_calls: Vec<StateToolCall>,
    /// The call a `TOOL_RESULT` answers.
    pub(crate) call_id: Option<String>,
    /// The tool a `TOOL_RESULT` came from.
    pub(crate) tool: Option<String>,
    /// Whether a `TOOL_RESULT` is a failure.
    pub(crate) is_error: Option<bool>,
    /// The checklist item it is, when it is one.
    pub(crate) checklist: Option<ChecklistItem>,
    /// The key it was written under, for a region that keys its entries.
    pub(crate) key: Option<String>,
    /// The model's reasoning that came with it.
    pub(crate) reasoning: Option<String>,
}

impl From<&EntryState> for ContextEntry {
    fn from(e: &EntryState) -> Self {
        let (kind, tool_calls, call_id, tool, is_error) = match &e.kind {
            EntryKind::Text => (ContextEntryKind::Text, Vec::new(), None, None, None),
            EntryKind::UserMessage => (ContextEntryKind::UserMessage, Vec::new(), None, None, None),
            EntryKind::AssistantTurn(calls) => (
                ContextEntryKind::AssistantTurn,
                calls.iter().map(StateToolCall::from).collect(),
                None,
                None,
                None,
            ),
            EntryKind::ToolResult {
                call_id,
                tool,
                is_error,
            } => (
                ContextEntryKind::ToolResult,
                Vec::new(),
                Some(call_id.clone()),
                Some(tool.clone()),
                Some(*is_error),
            ),
        };
        let checklist = match &e.meta {
            EntryMeta::None => None,
            EntryMeta::ChecklistItem { id, done, note } => Some(ChecklistItem {
                number: saturating(*id),
                done: *done,
                note: note.clone(),
            }),
        };
        Self {
            kind,
            text: e.text.clone(),
            parts: e.parts.iter().map(StatePart::from).collect(),
            tokens: saturating(e.tokens),
            timestamp: Timestamp(e.timestamp),
            tool_calls,
            call_id,
            tool,
            is_error,
            checklist,
            key: e.key.clone(),
            reasoning: e.reasoning.clone(),
        }
    }
}

/// One region of a run's context window.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ContextRegionState {
    /// The region.
    pub(crate) name: String,
    /// Its budget, in tokens.
    pub(crate) max_tokens: i32,
    /// What it holds now, in tokens.
    pub(crate) current_tokens: i32,
    /// Whether its conversation is due to be summarized.
    pub(crate) needs_message_compaction: bool,
    /// How sensitive what it holds is, when taint is tracked.
    pub(crate) taint: Option<RegionTaint>,
    /// Its entries, oldest first.
    pub(crate) entries: Vec<ContextEntry>,
}

impl From<&RegionState> for ContextRegionState {
    fn from(r: &RegionState) -> Self {
        Self {
            name: r.name.to_string(),
            max_tokens: saturating(r.max_tokens),
            current_tokens: saturating(r.current_tokens),
            needs_message_compaction: r.needs_message_compaction,
            taint: r.taint.as_ref().map(RegionTaint::from),
            entries: r.entries.iter().map(ContextEntry::from).collect(),
        }
    }
}

/// A run's context window: its regions, in the order a model reads them.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ContextState {
    /// The regions.
    pub(crate) regions: Vec<ContextRegionState>,
    /// Regions the current stage hides from the model.
    pub(crate) hidden: Vec<String>,
    /// The window's budget, in tokens.
    pub(crate) max_tokens: i32,
}

impl From<&CoreContext> for ContextState {
    fn from(c: &CoreContext) -> Self {
        Self {
            regions: c.regions.iter().map(ContextRegionState::from).collect(),
            hidden: c.hidden.iter().map(ToString::to_string).collect(),
            max_tokens: saturating(c.max_tokens),
        }
    }
}

/// Whether a step added entries to a region or replaced them all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum EntriesChangeMode {
    /// `entries` were added after the ones the region held.
    Append,
    /// `entries` are everything the region holds now.
    Replace,
}

/// What one step did to one region.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct RegionDiff {
    /// The region.
    pub(crate) name: String,
    /// Its budget after the step.
    pub(crate) max_tokens: i32,
    /// What it held after the step, in tokens.
    pub(crate) current_tokens: i32,
    /// Whether its conversation is due to be summarized.
    pub(crate) needs_message_compaction: bool,
    /// Its taint after the step.
    pub(crate) taint: Option<RegionTaint>,
    /// How its entries changed. Null when only the figures above did.
    pub(crate) mode: Option<EntriesChangeMode>,
    /// The entries added, or all of them.
    pub(crate) entries: Vec<ContextEntry>,
}

impl RegionDiff {
    /// One region's change.
    fn of(
        name: &leviath_runtime::spec::names::RegionName,
        head: &RegionHead,
        change: Option<&RegionChange>,
    ) -> Self {
        let typed = |entries: &[EntryState]| entries.iter().map(ContextEntry::from).collect();
        let (mode, entries) = match change {
            None => (None, Vec::new()),
            Some(RegionChange::Append(more)) => (Some(EntriesChangeMode::Append), typed(more)),
            Some(RegionChange::Replace(all)) => (Some(EntriesChangeMode::Replace), typed(all)),
        };
        Self {
            name: name.to_string(),
            max_tokens: saturating(head.max_tokens),
            current_tokens: saturating(head.current_tokens),
            needs_message_compaction: head.needs_message_compaction,
            taint: head.taint.as_ref().map(RegionTaint::from),
            mode,
            entries,
        }
    }
}

/// What one step did to a run's context window.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ContextDiff {
    /// Each region the step changed.
    pub(crate) regions: Vec<RegionDiff>,
    /// Regions the step removed.
    pub(crate) removed: Vec<String>,
    /// The regions' new order, when it changed.
    pub(crate) order: Option<Vec<String>>,
    /// The new hidden set, when it changed.
    pub(crate) hidden: Option<Vec<String>>,
    /// The window's new budget, when it changed.
    pub(crate) max_tokens: Option<i32>,
}

impl From<&CoreDiff> for ContextDiff {
    fn from(d: &CoreDiff) -> Self {
        let names = |list: &Vec<leviath_runtime::spec::names::RegionName>| {
            list.iter().map(ToString::to_string).collect::<Vec<_>>()
        };
        Self {
            regions: d
                .regions
                .iter()
                .map(|(name, head, change)| RegionDiff::of(name, head, change.as_ref()))
                .collect(),
            removed: names(&d.removed),
            order: d.order.as_ref().map(names),
            hidden: d.hidden.as_ref().map(names),
            max_tokens: d.max_tokens.map(saturating),
        }
    }
}
