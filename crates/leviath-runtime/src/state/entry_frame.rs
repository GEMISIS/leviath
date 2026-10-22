//! `EntryState`'s two forms: every field in a readable format, and the
//! binary one the run file keeps.
//!
//! The binary frame has the fields an entry had before it took `sticky`, so
//! run files keep their layout. A sticky entry writes its `meta` slot as the
//! variant added at the end of [`MetaFrame`], which carries the entry's own
//! meta inside it. An entry that is not sticky is written exactly as before,
//! byte for byte.

use serde::{Deserialize, Serialize};

use super::{EntryKind, EntryMeta, EntryState, PartState};
use crate::spec::readable::{WriteWith, pick};

/// The `meta` slot of an entry's binary frame.
///
/// Its first two variants keep the order and shape `EntryMeta` has always
/// been written in. `Sticky` is added at the end, so a file written before
/// it existed reads the same.
#[derive(Serialize, Deserialize)]
enum MetaFrame<'a> {
    None,
    ChecklistItem {
        id: u32,
        done: bool,
        note: Option<std::borrow::Cow<'a, str>>,
    },
    Sticky(std::borrow::Cow<'a, EntryMeta>),
}

impl<'a> MetaFrame<'a> {
    /// The slot for `meta` on an entry that is or is not sticky.
    fn new(meta: &'a EntryMeta, sticky: bool) -> Self {
        match (sticky, meta) {
            (true, meta) => MetaFrame::Sticky(std::borrow::Cow::Borrowed(meta)),
            (false, EntryMeta::None) => MetaFrame::None,
            (false, EntryMeta::ChecklistItem { id, done, note }) => MetaFrame::ChecklistItem {
                id: *id,
                done: *done,
                note: note.as_deref().map(std::borrow::Cow::Borrowed),
            },
        }
    }

    /// The meta and stickiness this slot holds.
    fn into_parts(self) -> (EntryMeta, bool) {
        match self {
            MetaFrame::None => (EntryMeta::None, false),
            MetaFrame::ChecklistItem { id, done, note } => (
                EntryMeta::ChecklistItem {
                    id,
                    done,
                    note: note.map(std::borrow::Cow::into_owned),
                },
                false,
            ),
            MetaFrame::Sticky(meta) => (meta.into_owned(), true),
        }
    }
}

/// An entry in the run file's binary frames, borrowed for writing.
#[derive(Serialize)]
struct EntryFrameRef<'a> {
    text: &'a str,
    parts: &'a [PartState],
    tokens: u32,
    timestamp: i64,
    kind: &'a EntryKind,
    meta: MetaFrame<'a>,
    key: &'a Option<String>,
    reasoning: &'a Option<String>,
}

/// An entry in the run file's binary frames, as read.
#[derive(Deserialize)]
struct EntryFrame {
    text: String,
    parts: Vec<PartState>,
    tokens: u32,
    timestamp: i64,
    kind: EntryKind,
    meta: MetaFrame<'static>,
    key: Option<String>,
    reasoning: Option<String>,
}

impl<'a> From<&'a EntryState> for EntryFrameRef<'a> {
    fn from(entry: &'a EntryState) -> Self {
        Self {
            text: &entry.text,
            parts: &entry.parts,
            tokens: entry.tokens,
            timestamp: entry.timestamp,
            kind: &entry.kind,
            meta: MetaFrame::new(&entry.meta, entry.sticky),
            key: &entry.key,
            reasoning: &entry.reasoning,
        }
    }
}

impl From<EntryFrame> for EntryState {
    fn from(frame: EntryFrame) -> Self {
        let (meta, sticky) = frame.meta.into_parts();
        Self {
            text: frame.text,
            parts: frame.parts,
            tokens: frame.tokens,
            timestamp: frame.timestamp,
            kind: frame.kind,
            meta,
            key: frame.key,
            reasoning: frame.reasoning,
            sticky,
        }
    }
}

impl Serialize for EntryState {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let frame: WriteWith<Self, S> = |v, s| EntryFrameRef::from(v).serialize(s);
        let text: WriteWith<Self, S> = |v, s| EntryState::serialize(v, s);
        pick(s.is_human_readable(), frame, text)(self, s)
    }
}

impl<'de> Deserialize<'de> for EntryState {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let frame: fn(D) -> Result<Self, D::Error> = |d| EntryFrame::deserialize(d).map(Self::from);
        let text: fn(D) -> Result<Self, D::Error> = |d| EntryState::deserialize(d);
        pick(d.is_human_readable(), frame, text)(d)
    }
}

#[cfg(test)]
#[path = "entry_frame_tests.rs"]
mod tests;
