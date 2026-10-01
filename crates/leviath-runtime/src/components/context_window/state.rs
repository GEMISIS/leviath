//! The window as a run's state records it, and back.
//!
//! [`ContextState`](crate::state::ContextState) keeps what changes as a run works: each region's entries,
//! budget, token count and taint, the hidden list and the window's budget. What
//! a region *is* (its kind, what it accepts, how it is cached) comes from the
//! run's graph, so rebuilding a window takes a `shape` for each region by name.

use leviath_core::mime::{BlobRef, MimeType, Part, PartBody};
use leviath_core::region::{EntryContent, RegionEntry, SerializedToolCall};
use leviath_core::{JsonDoc, Region};

use crate::spec::names::{Digest, RegionName};
use crate::state::ContextState;
use crate::state::context::{
    BlobState, EntryKind, EntryMeta, EntryState, PartBody as StatePartBody, PartState, RegionState,
    TaintState, ToolCallState,
};

use super::ContextWindow;

/// The metadata keys a checklist item is kept under, as
/// [`Region::add_checklist_item`] writes them.
const ITEM_ID: &str = "checklist_id";
const ITEM_DONE: &str = "checklist_done";
const ITEM_NOTE: &str = "checklist_note";

impl ContextWindow {
    /// The window as a run's state records it. A region whose name is not a
    /// valid region name is left out; every region a graph declares has one.
    pub fn to_state(&self) -> ContextState {
        let mut hidden: Vec<RegionName> = self
            .hidden
            .iter()
            .filter_map(|h| RegionName::new(h.as_str()).ok())
            .collect();
        hidden.sort();
        ContextState {
            regions: self.regions.iter().filter_map(region_state).collect(),
            hidden,
            max_tokens: small(self.max_tokens),
        }
    }

    /// A window holding what `state` records, each region shaped by `shape`
    /// (its kind and settings, from its name and recorded budget) with the
    /// recorded entries, token count, taint and compaction flag put back.
    pub(crate) fn from_state(state: &ContextState, shape: &dyn Fn(&str, usize) -> Region) -> Self {
        let mut window = ContextWindow::new(state.max_tokens as usize);
        for recorded in &state.regions {
            let mut region = shape(recorded.name.as_str(), recorded.max_tokens as usize);
            region.current_tokens = recorded.current_tokens as usize;
            region.needs_message_compaction = recorded.needs_message_compaction;
            region.content = recorded.entries.iter().map(entry).collect();
            region.taint = recorded
                .taint
                .as_ref()
                .map(|t| leviath_core::taint::RegionTaint::from_entry_taints(t.entries.clone()));
            window.regions.push(region);
        }
        window.hidden = state.hidden.iter().map(ToString::to_string).collect();
        window.current_tokens = window.calculate_tokens();
        window
    }
}

/// A count as the state keeps it, saturating rather than wrapping.
fn small(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn region_state(region: &Region) -> Option<RegionState> {
    Some(RegionState {
        name: RegionName::new(region.name.as_str()).ok()?,
        max_tokens: small(region.max_tokens),
        current_tokens: small(region.current_tokens),
        needs_message_compaction: region.needs_message_compaction,
        taint: region.taint.as_ref().map(|t| TaintState {
            level: t.level(),
            entries: (0..t.entry_count())
                .filter_map(|i| t.entry_taint(i))
                .collect(),
        }),
        entries: region.content.iter().map(entry_state).collect(),
    })
}

/// An entry as the state keeps it. Plain text keeps no parts: its text is the
/// whole of it. Anything else keeps every part, in order, text parts included.
fn entry_state(entry: &RegionEntry) -> EntryState {
    let text = entry.content.as_str();
    let plain = entry.content.parts() == [Part::text(text)];
    EntryState {
        text: text.to_string(),
        parts: match plain {
            true => Vec::new(),
            false => entry.content.parts().iter().map(part_state).collect(),
        },
        tokens: small(entry.tokens),
        timestamp: entry.timestamp,
        kind: match &entry.kind {
            leviath_core::EntryKind::Text => EntryKind::Text,
            leviath_core::EntryKind::UserMessage => EntryKind::UserMessage,
            leviath_core::EntryKind::AssistantTurn { tool_calls } => {
                EntryKind::AssistantTurn(tool_calls.iter().map(call_state).collect())
            }
            leviath_core::EntryKind::ToolResult {
                tool_call_id,
                tool_name,
                is_error,
            } => EntryKind::ToolResult {
                call_id: tool_call_id.clone(),
                tool: tool_name.clone(),
                is_error: *is_error,
            },
        },
        meta: entry
            .as_checklist_item()
            .map_or(EntryMeta::None, |item| EntryMeta::ChecklistItem {
                id: small(item.id),
                done: item.done,
                note: item.note,
            }),
        key: entry.key.clone(),
        reasoning: entry.reasoning.clone(),
    }
}

fn entry(state: &EntryState) -> RegionEntry {
    let parts: Vec<Part> = state.parts.iter().map(part).collect();
    RegionEntry {
        content: match parts.is_empty() {
            true => EntryContent::text(state.text.clone()),
            false => EntryContent::from_parts(parts),
        },
        tokens: state.tokens as usize,
        timestamp: state.timestamp,
        metadata: match &state.meta {
            EntryMeta::None => None,
            EntryMeta::ChecklistItem { id, done, note } => {
                let mut meta = serde_json::json!({ ITEM_ID: id, ITEM_DONE: done });
                if let (Some(note), serde_json::Value::Object(map)) = (note, &mut meta) {
                    map.insert(ITEM_NOTE.to_string(), note.clone().into());
                }
                Some(meta)
            }
        },
        kind: match &state.kind {
            EntryKind::Text => leviath_core::EntryKind::Text,
            EntryKind::UserMessage => leviath_core::EntryKind::UserMessage,
            EntryKind::AssistantTurn(calls) => leviath_core::EntryKind::AssistantTurn {
                tool_calls: calls.iter().map(call).collect(),
            },
            EntryKind::ToolResult {
                call_id,
                tool,
                is_error,
            } => leviath_core::EntryKind::ToolResult {
                tool_call_id: call_id.clone(),
                tool_name: tool.clone(),
                is_error: *is_error,
            },
        },
        key: state.key.clone(),
        reasoning: state.reasoning.clone(),
    }
}

fn call_state(call: &SerializedToolCall) -> ToolCallState {
    ToolCallState {
        id: call.id.clone(),
        name: call.name.clone(),
        args: JsonDoc::new(call.arguments.clone()),
        thought_signature: call.thought_signature.clone(),
    }
}

fn call(state: &ToolCallState) -> SerializedToolCall {
    SerializedToolCall {
        id: state.id.clone(),
        name: state.name.clone(),
        arguments: state.args.value().clone(),
        thought_signature: state.thought_signature.clone(),
    }
}

/// A part as the state keeps it. A stored part whose digest is not a sha256
/// (which the blob store never writes) is kept as its stand-in text.
pub(crate) fn part_state(part: &Part) -> PartState {
    let body = match &part.body {
        PartBody::Inline(text) => StatePartBody::Inline(text.clone()),
        PartBody::Stored(blob) => match Digest::new(blob.sha256.as_str()) {
            Ok(digest) => StatePartBody::Stored(BlobState {
                digest,
                size: blob.size,
                width: blob.width,
                height: blob.height,
                duration_ms: blob.duration_ms,
                tokens: small(blob.tokens),
                stand_in: blob.stand_in.clone(),
            }),
            Err(_) => StatePartBody::Inline(blob.stand_in.clone()),
        },
    };
    PartState {
        mime_type: part.mime_type.to_string(),
        body,
        name: part.name.clone(),
        deliver: part.deliver,
    }
}

/// A part as a window entry holds it. A mime type that no longer parses
/// becomes `application/octet-stream`, which every model is shown as a
/// stand-in.
pub(crate) fn part(state: &PartState) -> Part {
    let mime_type = MimeType::parse(&state.mime_type)
        .unwrap_or_else(|_| MimeType::parse("application/octet-stream").expect("a valid type"));
    let body = match &state.body {
        StatePartBody::Inline(text) => PartBody::Inline(text.clone()),
        StatePartBody::Stored(blob) => PartBody::Stored(BlobRef {
            sha256: blob.digest.to_string(),
            mime_type: mime_type.clone(),
            size: blob.size,
            width: blob.width,
            height: blob.height,
            duration_ms: blob.duration_ms,
            tokens: blob.tokens as usize,
            stand_in: blob.stand_in.clone(),
        }),
    };
    Part {
        mime_type,
        body,
        name: state.name.clone(),
        deliver: state.deliver,
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
