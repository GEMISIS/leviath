use super::*;
use crate::state::context::{PartBody, ToolCallState};

/// `EntryState` exactly as the run file wrote it before `sticky`: what a
/// layout-8 file written by an older build holds.
#[derive(Serialize, Deserialize)]
struct Before {
    text: String,
    parts: Vec<PartState>,
    tokens: u32,
    timestamp: i64,
    kind: EntryKind,
    meta: EntryMeta,
    key: Option<String>,
    reasoning: Option<String>,
}

fn entries() -> Vec<EntryState> {
    let plain = EntryState {
        text: "hello".into(),
        parts: vec![PartState {
            mime_type: "text/plain".into(),
            body: PartBody::Inline("hello".into()),
            name: Some("a.txt".into()),
            deliver: None,
        }],
        tokens: 2,
        timestamp: 1_700_000_000,
        kind: EntryKind::AssistantTurn(vec![ToolCallState {
            id: "c1".into(),
            name: "grep".into(),
            args: leviath_core::JsonDoc::new(serde_json::json!({ "q": "x" })),
            thought_signature: None,
        }]),
        meta: EntryMeta::None,
        key: Some("k".into()),
        reasoning: Some("sealed".into()),
        sticky: false,
    };
    let checklist = EntryState {
        meta: EntryMeta::ChecklistItem {
            id: 3,
            done: true,
            note: Some("shipped".into()),
        },
        kind: EntryKind::Text,
        ..plain.clone()
    };
    vec![plain, checklist]
}

fn before(entry: &EntryState) -> Before {
    Before {
        text: entry.text.clone(),
        parts: entry.parts.clone(),
        tokens: entry.tokens,
        timestamp: entry.timestamp,
        kind: entry.kind.clone(),
        meta: entry.meta.clone(),
        key: entry.key.clone(),
        reasoning: entry.reasoning.clone(),
    }
}

/// An entry that is not sticky is written byte for byte as before, and an
/// older file's entry reads back unchanged.
#[test]
fn an_unsticky_entry_keeps_the_layout_it_had() {
    for entry in entries() {
        let old = postcard::to_stdvec(&before(&entry)).unwrap();
        assert_eq!(postcard::to_stdvec(&entry).unwrap(), old);
        let read: EntryState = postcard::from_bytes(&old).unwrap();
        assert_eq!(read, entry);
    }
}

/// A sticky entry round-trips in both forms, whatever meta it carries.
#[test]
fn a_sticky_entry_round_trips() {
    for mut entry in entries() {
        entry.sticky = true;
        let bytes = postcard::to_stdvec(&entry).unwrap();
        let read: EntryState = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(read, entry);

        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["sticky"], true);
        let read: EntryState = serde_json::from_value(json).unwrap();
        assert_eq!(read, entry);
    }
}

/// The readable form leaves `sticky` out when it is false, and reads an
/// entry written without it.
#[test]
fn the_readable_form_mentions_sticky_only_when_set() {
    let entry = entries().remove(0);
    let json = serde_json::to_value(&entry).unwrap();
    assert!(json.get("sticky").is_none(), "{json}");
    let read: EntryState = serde_json::from_value(json).unwrap();
    assert!(!read.sticky);
}
