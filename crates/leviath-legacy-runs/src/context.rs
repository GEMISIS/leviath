//! An old context snapshot, read as the run file's typed context.

use std::collections::BTreeSet;

use leviath_core::JsonDoc;
use leviath_core::mime::{Part, PartBody as OldBody, text_plain};
use leviath_core::region::{EntryContent, EntryKind as OldKind};
use leviath_core::run_meta::{ContextSnapshot, RegionEntrySnapshot, RegionSnapshot};
use leviath_core::taint::TaintLevel;
use leviath_runtime::spec::names::{Digest, RegionName};
use leviath_runtime::state::context::{BlobState, PartBody, PartState, TaintState, ToolCallState};
use leviath_runtime::state::{ContextState, EntryKind, EntryMeta, EntryState, RegionState};

use crate::report::Report;

/// The metadata keys a checklist item carries.
const ITEM_ID: &str = "checklist_id";
const ITEM_DONE: &str = "checklist_done";
const ITEM_NOTE: &str = "checklist_note";

/// A count clamped into a `u32`.
pub(crate) fn n32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// What reading a snapshot could not carry over.
#[derive(Debug, Default)]
pub(crate) struct Losses {
    /// Regions whose names are not valid region names, left out.
    regions: BTreeSet<String>,
    /// Entries whose metadata was not a checklist item.
    metadata: usize,
    /// Stored parts whose digests are not valid, kept as their stand-in text.
    digests: BTreeSet<String>,
}

impl Losses {
    /// Name every loss in the report.
    pub(crate) fn report(&self, report: &mut Report) {
        for name in &self.regions {
            report.note(format!(
                "context region {name:?} was left out: not a valid region name"
            ));
        }
        if self.metadata > 0 {
            report.fill(
                "context.entries.meta",
                "None",
                format!(
                    "{} entries carried metadata other than a checklist item, which the run file has no place for",
                    self.metadata
                ),
            );
        }
        for d in &self.digests {
            report.note(format!(
                "a stored part named {d:?} is kept as its stand-in text: not a valid digest"
            ));
        }
    }
}

/// Read a snapshot. `hidden` is the current stage's hidden regions, and
/// `taint` whether taint tracking is on.
pub(crate) fn state(
    snapshot: &ContextSnapshot,
    hidden: Vec<RegionName>,
    taint: bool,
    losses: &mut Losses,
) -> ContextState {
    let regions = snapshot
        .regions
        .iter()
        .filter_map(|r| region(r, taint, losses))
        .collect();
    ContextState {
        regions,
        hidden,
        max_tokens: n32(snapshot.max_tokens),
    }
}

fn region(r: &RegionSnapshot, taint: bool, losses: &mut Losses) -> Option<RegionState> {
    let Ok(name) = RegionName::new(r.name.as_str()) else {
        losses.regions.insert(r.name.clone());
        return None;
    };
    let levels: Vec<TaintLevel> = r.entries.iter().map(|e| e.taint).collect();
    let taint = taint.then(|| TaintState {
        level: levels.iter().fold(TaintLevel::Public, |a, b| a.max(*b)),
        entries: levels,
    });
    Some(RegionState {
        name,
        max_tokens: n32(r.max_tokens),
        current_tokens: n32(r.current_tokens),
        needs_message_compaction: false,
        taint,
        entries: r.entries.iter().map(|e| entry(e, losses)).collect(),
    })
}

fn entry(e: &RegionEntrySnapshot, losses: &mut Losses) -> EntryState {
    EntryState {
        text: e.content.as_str().to_string(),
        parts: parts(&e.content, losses),
        tokens: n32(e.tokens),
        timestamp: 0,
        kind: kind(&e.kind),
        meta: meta(e.metadata.as_ref(), losses),
        key: e.key.clone(),
        reasoning: e.reasoning.clone(),
    }
}

fn kind(k: &OldKind) -> EntryKind {
    match k {
        OldKind::Text => EntryKind::Text,
        OldKind::UserMessage => EntryKind::UserMessage,
        OldKind::AssistantTurn { tool_calls } => EntryKind::AssistantTurn(
            tool_calls
                .iter()
                .map(|c| ToolCallState {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    args: JsonDoc::new(c.arguments.clone()),
                    thought_signature: c.thought_signature.clone(),
                })
                .collect(),
        ),
        OldKind::ToolResult {
            tool_call_id,
            tool_name,
            is_error,
        } => EntryKind::ToolResult {
            call_id: tool_call_id.clone(),
            tool: tool_name.clone(),
            is_error: *is_error,
        },
    }
}

fn meta(metadata: Option<&serde_json::Value>, losses: &mut Losses) -> EntryMeta {
    let Some(m) = metadata else {
        return EntryMeta::None;
    };
    match m.get(ITEM_ID).and_then(serde_json::Value::as_u64) {
        Some(id) => EntryMeta::ChecklistItem {
            id: u32::try_from(id).unwrap_or(u32::MAX),
            done: m.get(ITEM_DONE).and_then(serde_json::Value::as_bool) == Some(true),
            note: m
                .get(ITEM_NOTE)
                .and_then(|n| n.as_str())
                .map(str::to_string),
        },
        None => {
            losses.metadata += 1;
            EntryMeta::None
        }
    }
}

/// An entry's typed parts. Plain text alone is the entry's text and needs no
/// part of its own.
pub(crate) fn parts(content: &EntryContent, losses: &mut Losses) -> Vec<PartState> {
    let parts = content.parts();
    let plain =
        matches!(parts, [p] if p.mime_type == text_plain() && matches!(p.body, OldBody::Inline(_)));
    match plain {
        true => Vec::new(),
        false => parts.iter().map(|p| part(p, losses)).collect(),
    }
}

fn part(p: &Part, losses: &mut Losses) -> PartState {
    let body = match &p.body {
        OldBody::Inline(text) => PartBody::Inline(text.clone()),
        OldBody::Stored(blob) => match Digest::new(blob.sha256.as_str()) {
            Ok(digest) => PartBody::Stored(BlobState {
                digest,
                size: blob.size,
                width: blob.width,
                height: blob.height,
                duration_ms: blob.duration_ms,
                tokens: n32(blob.tokens),
                stand_in: blob.stand_in.clone(),
            }),
            Err(_) => {
                losses.digests.insert(blob.sha256.clone());
                PartBody::Inline(blob.stand_in.clone())
            }
        },
    };
    PartState {
        mime_type: p.mime_type.as_str().to_string(),
        body,
        name: p.name.clone(),
        deliver: p.deliver,
    }
}
