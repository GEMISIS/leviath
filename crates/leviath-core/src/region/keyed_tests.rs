//! Budgets, recency and stickiness on keyed writes.

use super::*;

fn keyed(max_entries: Option<usize>, max_tokens: usize) -> Region {
    Region::new(
        "tasks".to_string(),
        RegionKind::HashMap { max_entries },
        max_tokens,
    )
}

/// Replacing a key with something bigger must still keep the region inside
/// its budget, by evicting the least recently written other key.
#[test]
fn replacing_a_key_with_a_larger_entry_keeps_the_budget() {
    let mut region = keyed(None, 30);
    region.upsert_by_key("a", "a", 10).unwrap();
    region.upsert_by_key("b", "b", 10).unwrap();
    region.upsert_by_key("c", "c", 10).unwrap();

    region.upsert_by_key("c", "c2", 20).unwrap();

    assert!(region.current_tokens <= region.max_tokens);
    assert_eq!(region.current_tokens, 30);
    assert!(region.get_by_key("a").is_none());
    assert_eq!(region.get_by_key("c").unwrap().content, "c2");
}

/// A new entry larger than the whole region fails without evicting
/// anything first.
#[test]
fn an_oversized_new_entry_fails_and_leaves_the_region_untouched() {
    let mut region = keyed(None, 30);
    region.upsert_by_key("a", "a", 10).unwrap();
    region.upsert_by_key("b", "b", 10).unwrap();

    assert!(region.upsert_by_key("huge", "x", 31).is_err());

    assert_eq!(region.keys(), vec!["a", "b"]);
    assert_eq!(region.current_tokens, 20);
}

/// The same for a replacement: a value the region can never hold fails and
/// the old value stays.
#[test]
fn an_oversized_replacement_fails_and_keeps_the_old_value() {
    let mut region = keyed(None, 30);
    region.upsert_by_key("a", "a", 10).unwrap();
    region.upsert_by_key("b", "b", 10).unwrap();

    assert!(region.upsert_by_key("b", "b2", 31).is_err());

    assert_eq!(region.get_by_key("b").unwrap().content, "b");
    assert_eq!(region.current_tokens, 20);
    assert_eq!(region.entry_count(), 2);
}

/// `add_keyed_entry` keeps to `max_entries` the way `upsert_by_key` does.
#[test]
fn add_keyed_entry_honours_max_entries() {
    let mut region = keyed(Some(2), 1000);
    region.add_keyed_entry("a", "a", 1).unwrap();
    region.add_keyed_entry("b", "b", 1).unwrap();
    region.add_keyed_entry("c", "c", 1).unwrap();

    assert_eq!(region.entry_count(), 2);
    assert_eq!(region.keys(), vec!["b", "c"]);
}

/// Writes in the same second still have an order: the least recently
/// written key goes first even when every timestamp ties.
#[test]
fn recency_breaks_same_second_ties_by_write_order() {
    let mut region = keyed(Some(2), 1000);
    region.upsert_by_key("a", "a", 1).unwrap();
    region.upsert_by_key("b", "b", 1).unwrap();
    // Rewriting `a` makes `b` the least recently written.
    region.upsert_by_key("a", "a2", 1).unwrap();
    for entry in &mut region.content {
        entry.timestamp = 1_000;
    }

    region.upsert_by_key("c", "c", 1).unwrap();

    assert!(region.get_by_key("b").is_none());
    assert!(region.get_by_key("a").is_some());
}

/// A keyed region's description reaches the model when it asks for it.
#[test]
fn a_keyed_region_renders_its_description_when_asked() {
    let mut region = keyed(None, 1000);
    region.description = Some("One line per open task.".to_string());
    region.describe_in_prompt = true;
    region.upsert_by_key("t1", "write the parser", 5).unwrap();

    let mut blocks = Vec::new();
    crate::context::push_region_blocks(&mut blocks, &region);

    assert_eq!(
        blocks[0].text,
        "[tasks]:\nOne line per open task.\n\n### [t1]\nwrite the parser"
    );
}

/// Every write stamps one past the newest, and an upsert stamps again.
#[test]
fn every_write_takes_the_next_sequence_number() {
    let mut region = keyed(None, 1000);
    region.upsert_by_key("a", "a", 1).unwrap();
    region.add_keyed_entry("b", "b", 1).unwrap();
    region.upsert_by_key("a", "a2", 1).unwrap();
    let seqs: Vec<u64> = region.content.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, [3, 2]);
}

/// A sticky entry outlives every eviction a keyed region makes, however old.
#[test]
fn a_sticky_entry_is_never_evicted_for_room() {
    let mut region = keyed(Some(2), 30);
    region
        .upsert_by_key_sticky("running", "r", 10, true)
        .unwrap();
    region.upsert_by_key("b", "b", 10).unwrap();
    // The entry cap: `b` goes, not the older `running`.
    region.upsert_by_key("c", "c", 10).unwrap();
    assert_eq!(region.keys(), ["running", "c"]);
    // The token budget: `c` goes for a bigger `d`.
    region.upsert_by_key("d", "d", 20).unwrap();
    assert_eq!(region.keys(), ["running", "d"]);
    assert_eq!(region.current_tokens, 30);
    // A plain upsert of a sticky key keeps it sticky.
    region.upsert_by_key("running", "r2", 10).unwrap();
    assert!(region.get_by_key("running").unwrap().sticky);
    region
        .upsert_by_key_sticky("running", "r3", 10, false)
        .unwrap();
    assert!(!region.get_by_key("running").unwrap().sticky);
}

/// A write that only fits by evicting a sticky entry fails, and says why.
#[test]
fn a_write_that_needs_a_sticky_entry_gone_fails_untouched() {
    let mut region = keyed(None, 30);
    region.upsert_by_key_sticky("a", "a", 20, true).unwrap();
    region.upsert_by_key("b", "b", 5).unwrap();

    let err = region.upsert_by_key("c", "c", 15).unwrap_err();
    assert!(err.contains("sticky entries hold 20 tokens"), "{err}");
    assert_eq!(region.keys(), ["a", "b"]);

    // A replacement too big beside the other sticky entries.
    region.upsert_by_key_sticky("b", "b", 5, true).unwrap();
    assert!(region.upsert_by_key("b", "b2", 11).is_err());
    assert_eq!(region.get_by_key("b").unwrap().content, "b");
}

/// The entry cap refuses when only sticky entries could make a slot.
#[test]
fn an_entry_cap_full_of_sticky_entries_refuses_a_new_key() {
    let mut region = keyed(Some(1), 100);
    region.upsert_by_key_sticky("a", "a", 1, true).unwrap();
    let err = region.upsert_by_key("b", "b", 1).unwrap_err();
    assert!(err.contains("1-entry limit"), "{err}");
    let refused = region.add_keyed_entry("b", "b", 1).unwrap_err();
    assert!(
        matches!(refused, crate::error::Error::RegionRefusedWrite { .. }),
        "{refused:?}"
    );
    assert_eq!(region.keys(), ["a"]);
    // Replacing the sticky key needs no slot.
    region.upsert_by_key("a", "a2", 1).unwrap();
}

/// Under `reject`, a full entry cap refuses rather than evicting.
#[test]
fn a_rejecting_region_refuses_past_its_entry_cap() {
    let mut region = keyed(Some(1), 100);
    region.admission = Admission::Reject;
    region.add_keyed_entry("a", "a", 1).unwrap();
    let refused = region.add_keyed_entry("b", "b", 1).unwrap_err();
    assert!(
        matches!(refused, crate::error::Error::RegionFull { .. }),
        "{refused:?}"
    );
    assert_eq!(region.keys(), ["a"]);
}

#[test]
fn set_sticky_marks_an_existing_key_only() {
    let mut region = keyed(None, 100);
    region.upsert_by_key("a", "a", 1).unwrap();
    assert!(region.set_sticky("a", true));
    assert!(region.get_by_key("a").unwrap().sticky);
    assert!(!region.set_sticky("missing", true));
    // Naming a sticky entry still removes it.
    assert!(region.remove_by_key("a"));
    assert!(!region.remove_by_key("a"));
}

/// A store that keeps no sequence gets one from timestamps, then position.
#[test]
fn restamped_recency_follows_timestamps_then_position() {
    let mut region = keyed(None, 100);
    for key in ["a", "b", "c"] {
        region.upsert_by_key(key, key, 1).unwrap();
    }
    region.content[0].timestamp = 300;
    region.content[1].timestamp = 100;
    region.content[2].timestamp = 100;
    for entry in &mut region.content {
        entry.seq = 0;
    }
    region.restamp_recency();
    let seqs: Vec<u64> = region.content.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, [3, 1, 2]);
}

/// An entry built by hand with no sequence counts as older than any the
/// region stamped, whatever its timestamp says.
#[test]
fn an_unstamped_entry_is_the_least_recent() {
    let mut region = keyed(Some(2), 100);
    region.upsert_by_key("stamped", "s", 1).unwrap();
    region.content.insert(
        0,
        RegionEntry {
            content: "old".into(),
            tokens: 1,
            timestamp: i64::MAX,
            metadata: None,
            kind: EntryKind::Text,
            key: Some("manual".into()),
            reasoning: None,
            seq: 0,
            sticky: false,
        },
    );
    region.current_tokens += 1;
    region.upsert_by_key("new", "n", 1).unwrap();
    assert_eq!(region.keys(), ["stamped", "new"]);
}

/// Oldest-first eviction leaves sticky entries where they are.
#[test]
fn oldest_first_eviction_passes_over_sticky_entries() {
    let mut region = Region::new("scratch".into(), RegionKind::Temporary, 100);
    region.enable_taint_tracking();
    region.add_entry("keep", 10).unwrap();
    region.content[0].sticky = true;
    region
        .add_typed_entry(
            "calling",
            5,
            EntryKind::AssistantTurn {
                tool_calls: vec![SerializedToolCall {
                    id: "c1".into(),
                    name: "grep".into(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                }],
            },
        )
        .unwrap();
    region
        .add_typed_entry(
            "found",
            5,
            EntryKind::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "grep".into(),
                is_error: false,
            },
        )
        .unwrap();
    region.add_entry("note", 3).unwrap();

    // The turn group leaves whole, its tokens on the returned entry.
    let removed = region.remove_oldest().unwrap();
    assert_eq!(removed.content, "calling");
    assert_eq!(removed.tokens, 10);
    assert_eq!(region.current_tokens, 13);
    assert_eq!(region.taint.as_ref().unwrap().entry_count(), 2);

    assert_eq!(region.remove_oldest().unwrap().content, "note");
    assert!(region.remove_oldest().is_none());
    assert_eq!(region.content.len(), 1);
}

#[test]
fn make_room_does_not_start_when_sticky_entries_leave_too_little() {
    let mut region = Region::new("scratch".into(), RegionKind::Temporary, 20);
    region.add_entry("keep", 15).unwrap();
    region.content[0].sticky = true;
    region.add_entry("loose", 5).unwrap();
    assert_eq!(region.sticky_tokens(), 15);
    assert_eq!(region.make_room(6), 0);
    assert_eq!(region.content.len(), 2);
    assert_eq!(region.make_room(5), 1);
    assert_eq!(region.content.len(), 1);
}

#[test]
fn evict_all_keeps_sticky_entries_and_clear_does_not() {
    let mut region = Region::new("dump".into(), RegionKind::Clearable, 100);
    region.enable_taint_tracking();
    region.add_entry("a", 1).unwrap();
    region.add_entry("keep", 2).unwrap();
    region.content[1].sticky = true;
    region.add_entry("c", 3).unwrap();
    assert_eq!(region.evict_all(), 4);
    assert_eq!(region.content.len(), 1);
    assert_eq!(region.current_tokens, 2);
    assert_eq!(region.taint.as_ref().unwrap().entry_count(), 1);
    region.clear();
    assert!(region.content.is_empty());

    // The same with no taint to keep in step.
    let mut untracked = Region::new("dump".into(), RegionKind::Clearable, 100);
    untracked.add_entry("a", 1).unwrap();
    assert_eq!(untracked.evict_all(), 1);
}

/// A sticky entry also survives a sliding window's roll-off.
#[test]
fn a_sliding_window_rolls_off_around_a_sticky_entry() {
    let mut region = Region::new(
        "conversation".into(),
        RegionKind::SlidingWindow {
            max_items: 2,
            eviction_strategy: EvictionStrategy::PerItem,
        },
        100,
    );
    region.add_entry("first", 1).unwrap();
    region.content[0].sticky = true;
    region.add_entry("second", 1).unwrap();
    region.add_entry("third", 1).unwrap();
    let texts: Vec<&str> = region.content.iter().map(|e| e.content.as_str()).collect();
    assert_eq!(texts, ["first", "third"]);
}

/// A bracketed region shows its description too, on its first chunk.
#[test]
fn a_temporary_region_renders_its_description_when_asked() {
    let mut region = Region::new("scratch".into(), RegionKind::Temporary, 1000);
    region.add_entry("working", 2).unwrap();
    let mut plain = Vec::new();
    crate::context::push_region_blocks(&mut plain, &region);
    assert_eq!(plain[0].text, "[scratch]:\nworking");

    region.description = Some("Throwaway notes.".into());
    let mut undescribed = Vec::new();
    crate::context::push_region_blocks(&mut undescribed, &region);
    assert_eq!(undescribed[0].text, "[scratch]:\nworking");

    region.describe_in_prompt = true;
    let mut described = Vec::new();
    crate::context::push_region_blocks(&mut described, &region);
    assert_eq!(described[0].text, "[scratch]:\nThrowaway notes.\n\nworking");
}

// ─── replace_keyed_entry: a result the same call produced again ─────────────

fn growing(max_tokens: usize) -> Region {
    Region::new(
        "codebase".to_string(),
        RegionKind::Compacting {
            threshold_tokens: max_tokens,
        },
        max_tokens,
    )
}

fn replace(region: &mut Region, key: &str, body: &str, tokens: usize) -> KeyedWrite {
    region
        .replace_keyed_entry(
            key,
            EntryContent::text(body),
            tokens,
            EntryKind::Text,
            crate::taint::TaintLevel::Public,
        )
        .unwrap()
}

fn held(region: &Region) -> Vec<&str> {
    region.content.iter().map(|e| e.content.as_str()).collect()
}

/// Re-reading a path leaves one entry, not two: the same content stays
/// exactly where it was, and changed content replaces it as the newest.
#[test]
fn a_repeated_read_leaves_one_entry_under_its_key() {
    let mut region = growing(100);
    assert_eq!(
        replace(&mut region, "read_file a.rs", "fn a() {}", 10),
        KeyedWrite::Added
    );
    assert_eq!(
        replace(&mut region, "read_file b.rs", "fn b() {}", 10),
        KeyedWrite::Added
    );
    let first_seq = region.get_by_key("read_file a.rs").unwrap().seq;

    // The same bytes again: nothing moves, so the prompt is what it was, but
    // the entry counts as the most recently written.
    assert_eq!(
        replace(&mut region, "read_file a.rs", "fn a() {}", 10),
        KeyedWrite::Unchanged
    );
    assert_eq!(held(&region), ["fn a() {}", "fn b() {}"]);
    assert_eq!(region.current_tokens, 20);
    assert!(region.get_by_key("read_file a.rs").unwrap().seq > first_seq);

    // The file changed: the earlier copy goes and the new one is the newest.
    assert_eq!(
        replace(&mut region, "read_file a.rs", "fn a() { 1 }", 12),
        KeyedWrite::Replaced
    );
    assert_eq!(held(&region), ["fn b() {}", "fn a() { 1 }"]);
    assert_eq!(region.current_tokens, 22);
    assert_eq!(region.keys(), ["read_file b.rs", "read_file a.rs"]);
}

/// The same text under a different kind is a different entry: a tool result
/// and a plain note are not interchangeable to the assembler.
#[test]
fn the_same_text_with_another_kind_replaces_rather_than_matches() {
    let mut region = growing(100);
    replace(&mut region, "k", "body", 4);
    let written = region
        .replace_keyed_entry(
            "k",
            EntryContent::text("body"),
            4,
            EntryKind::UserMessage,
            crate::taint::TaintLevel::Public,
        )
        .unwrap();
    assert_eq!(written, KeyedWrite::Replaced);
    assert_eq!(region.content.len(), 1);
    assert_eq!(region.content[0].kind, EntryKind::UserMessage);
}

/// The freed tokens of the replaced entry count as room, and the taint vector
/// follows the entries so a tracked region stays aligned with its content.
#[test]
fn a_replacement_reuses_the_room_its_predecessor_held() {
    let mut region = growing(100);
    region.enable_taint_tracking();
    replace(&mut region, "old", "o", 30);
    replace(&mut region, "k", "first", 60);
    // 90 held; a 50-token replacement of `k` fits once `first` (60) is gone
    // without evicting `old`.
    assert_eq!(
        replace(&mut region, "k", "second", 50),
        KeyedWrite::Replaced
    );
    assert_eq!(held(&region), ["o", "second"]);
    assert_eq!(region.current_tokens, 80);
    assert_eq!(region.taint.as_ref().unwrap().entry_count(), 2);
}

/// Content the region does not take is refused before anything goes, so the
/// earlier copy under the key is still there.
#[test]
fn a_refused_replacement_keeps_the_earlier_entry() {
    use crate::mime::{Blob, BlobStore, MemoryBlobStore, MimeRegistry, MimeType, Part};

    let mut region = growing(100);
    region.accepts = vec!["text/*".to_string()];
    replace(&mut region, "k", "text", 4);
    let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![1, 2, 3]);
    let stored = MemoryBlobStore::new()
        .put("r", &blob, &MimeRegistry::builtin())
        .unwrap();
    let png = EntryContent::from_parts(vec![Part::text("caption"), Part::stored(stored)]);
    let err = region
        .replace_keyed_entry(
            "k",
            png,
            4,
            EntryKind::Text,
            crate::taint::TaintLevel::Public,
        )
        .unwrap_err();
    assert!(
        matches!(err, crate::error::Error::RegionRefusedWrite { .. }),
        "{err}"
    );
    assert_eq!(held(&region), ["text"]);
}

/// A replacement larger than the whole region fits nowhere even with the
/// earlier copy gone: the key is left empty for the caller's truncated copy,
/// which is exactly what a first write that size would get.
#[test]
fn a_replacement_that_fits_nowhere_leaves_the_key_empty() {
    let mut region = growing(100);
    replace(&mut region, "k", "small", 10);
    let err = region
        .replace_keyed_entry(
            "k",
            EntryContent::text("enormous"),
            500,
            EntryKind::Text,
            crate::taint::TaintLevel::Public,
        )
        .unwrap_err();
    assert!(
        matches!(err, crate::error::Error::TokenBudgetExceeded { .. }),
        "{err}"
    );
    assert!(region.get_by_key("k").is_none());
    assert!(region.content.is_empty());
}
