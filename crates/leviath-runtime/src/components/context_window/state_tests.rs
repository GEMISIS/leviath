use super::*;
use leviath_core::{EntryKind as CoreKind, RegionKind};

fn sha() -> String {
    "a".repeat(64)
}

fn stored_part() -> Part {
    Part {
        mime_type: MimeType::parse("image/png").unwrap(),
        body: PartBody::Stored(BlobRef {
            sha256: sha(),
            mime_type: MimeType::parse("image/png").unwrap(),
            size: 3,
            width: Some(2),
            height: Some(1),
            duration_ms: None,
            tokens: 85,
            stand_in: "[image/png, 3 B]".to_string(),
        }),
        name: Some("a.png".to_string()),
        deliver: Some(leviath_core::mime::Delivery::StandIn),
    }
}

/// A window holding one of every kind of entry the state keeps: plain text, a
/// person's message, a model turn with a call, its result, a checklist item
/// with and without a note, a keyed entry with reasoning, and a stored part.
fn full_window() -> ContextWindow {
    let mut w = ContextWindow::new(20_000);
    w.add_region(Region::new("notes".into(), RegionKind::Checklist, 4000));
    w.add_region(Region::new(
        "conversation".into(),
        RegionKind::Clearable,
        10_000,
    ));
    let notes = w.get_region_mut("notes").unwrap();
    let one = notes.add_checklist_item("one".into(), 2).unwrap();
    notes.add_checklist_item("two".into(), 2).unwrap();
    notes.complete_checklist_item(one);
    notes.note_checklist_item(one, "done early");
    let conv = w.get_region_mut("conversation").unwrap();
    conv.add_entry("plain".to_string(), 2).unwrap();
    conv.enable_taint_tracking();
    conv.content.push(RegionEntry {
        content: EntryContent::text("hi"),
        tokens: 1,
        timestamp: 5,
        metadata: None,
        kind: CoreKind::UserMessage,
        key: Some("k".into()),
        reasoning: Some("why".into()),
    });
    conv.content.push(RegionEntry {
        content: EntryContent::text("calling"),
        tokens: 1,
        timestamp: 6,
        metadata: None,
        kind: CoreKind::AssistantTurn {
            tool_calls: vec![SerializedToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "a"}),
                thought_signature: Some("sig".into()),
            }],
        },
        key: None,
        reasoning: None,
    });
    conv.content.push(RegionEntry {
        content: EntryContent::from_parts(vec![Part::text("see"), stored_part()]),
        tokens: 90,
        timestamp: 7,
        metadata: None,
        kind: CoreKind::ToolResult {
            tool_call_id: "c1".into(),
            tool_name: "read_file".into(),
            is_error: true,
        },
        key: None,
        reasoning: None,
    });
    conv.needs_message_compaction = true;
    w.hidden.insert("notes".to_string());
    w.current_tokens = w.calculate_tokens();
    w
}

fn shape(name: &str, max_tokens: usize) -> Region {
    match name {
        "notes" => Region::new(name.into(), RegionKind::Checklist, max_tokens),
        _ => Region::new(name.into(), RegionKind::Clearable, max_tokens),
    }
}

#[test]
fn a_window_survives_the_trip_through_its_state() {
    let w = full_window();
    let state = w.to_state();
    assert_eq!(state.max_tokens, 20_000);
    assert_eq!(state.hidden, vec![RegionName::new("notes").unwrap()]);
    let conv = state.region("conversation").unwrap();
    assert!(conv.needs_message_compaction);
    assert!(conv.taint.is_some());
    assert!(
        matches!(conv.entries[2].kind, EntryKind::AssistantTurn(ref calls) if calls[0].id == "c1")
    );
    assert!(matches!(
        state.region("notes").unwrap().entries[0].meta,
        EntryMeta::ChecklistItem {
            id: 1,
            done: true,
            note: Some(_)
        }
    ));

    let back = ContextWindow::from_state(&state, &shape);
    assert_eq!(back.to_state(), state, "nothing is lost on the way back");
    assert_eq!(back.current_tokens, w.current_tokens);
    assert_eq!(back.hidden, w.hidden);
    let notes = back.get_region("notes").unwrap();
    assert_eq!(notes.open_checklist_items().len(), 1);
    assert_eq!(
        notes.checklist_items()[0].note.as_deref(),
        Some("done early")
    );
    let conv = back.get_region("conversation").unwrap();
    assert_eq!(
        conv.content[3].content.parts(),
        w.get_region("conversation").unwrap().content[3]
            .content
            .parts()
    );
}

#[test]
fn a_region_with_a_name_no_graph_could_declare_is_left_out() {
    let mut w = ContextWindow::new(100);
    w.add_region(Region::new(" bad".into(), RegionKind::Pinned, 10));
    w.hidden.insert(" bad".into());
    let state = w.to_state();
    assert!(state.regions.is_empty() && state.hidden.is_empty());
}

#[test]
fn counts_past_what_the_state_holds_saturate() {
    assert_eq!(small(usize::MAX), u32::MAX);
    assert_eq!(small(7), 7);
}

#[test]
fn a_part_with_a_broken_digest_or_type_still_reads_back() {
    let mut odd = stored_part();
    if let PartBody::Stored(blob) = &mut odd.body {
        blob.sha256 = "not-a-digest".into();
    }
    let state = part_state(&odd);
    assert_eq!(state.body, StatePartBody::Inline("[image/png, 3 B]".into()));

    let mut garbled = part_state(&stored_part());
    garbled.mime_type = "not a type".into();
    let back = part(&garbled);
    assert_eq!(back.mime_type.as_str(), "application/octet-stream");
    assert!(matches!(back.body, PartBody::Stored(ref b) if b.sha256 == sha()));
}
