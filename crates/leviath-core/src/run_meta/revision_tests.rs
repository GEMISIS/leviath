//! What a revision has to promise, stated as tests.

use super::*;
use crate::region::{EntryContent, EntryKind};
use crate::run_meta::{ContextSnapshot, RegionEntrySnapshot, RegionSnapshot};

/// One plain text entry.
fn entry(text: &str, tokens: usize) -> RegionEntrySnapshot {
    RegionEntrySnapshot {
        content: EntryContent::from(text),
        tokens,
        kind: EntryKind::Text,
        metadata: None,
        key: None,
        taint: Default::default(),
        reasoning: None,
    }
}

/// One region holding `entries`, its token count their sum.
fn region(name: &str, entries: Vec<RegionEntrySnapshot>) -> RegionSnapshot {
    let current_tokens = entries.iter().map(|e| e.tokens).sum();
    RegionSnapshot {
        name: name.to_string(),
        kind: "clearable".to_string(),
        current_tokens,
        max_tokens: 1_000,
        entries,
        description: None,
    }
}

/// One window holding `regions`.
fn window(regions: Vec<RegionSnapshot>) -> ContextSnapshot {
    let total_tokens = regions.iter().map(|r| r.current_tokens).sum();
    ContextSnapshot {
        stage_name: "gather".to_string(),
        total_tokens,
        max_tokens: 10_000,
        regions,
    }
}

/// The same content gets the same revision however often it is asked for, and
/// the revision says which scheme produced it.
#[test]
fn one_window_has_one_revision() {
    let snapshot = window(vec![region("conv", vec![entry("hello", 2)])]);
    let first = context_revision(&snapshot);
    assert_eq!(first, context_revision(&snapshot.clone()));
    assert!(first.starts_with("cw1-"), "{first}");
    assert_eq!(first.len(), "cw1-".len() + DIGEST_HEX);
    let hex = first.strip_prefix("cw1-").expect("the scheme, then hex");
    assert!(
        hex.chars().all(|c| c.is_ascii_hexdigit()),
        "hex after the scheme: {first}"
    );
}

/// The stage is not part of the identity. A window does not know its stage name
/// where a change is recorded, so a revision that used it could never be
/// recomputed there.
#[test]
fn the_stage_is_not_part_of_a_windows_identity() {
    let snapshot = window(vec![region("conv", vec![entry("hello", 2)])]);
    let elsewhere = ContextSnapshot {
        stage_name: "review".to_string(),
        ..snapshot.clone()
    };
    assert_eq!(context_revision(&snapshot), context_revision(&elsewhere));
}

/// Every fact a window is made of moves its revision. Listed one at a time
/// rather than asserted in a lump, so a change that stops counting names itself.
#[test]
fn every_fact_about_a_window_moves_its_revision() {
    let base = window(vec![region("conv", vec![entry("hello", 2)])]);
    let revision = context_revision(&base);

    let mut renamed = base.clone();
    renamed.regions[0].name = "notes".to_string();
    let mut rekinded = base.clone();
    rekinded.regions[0].kind = "pinned".to_string();
    let mut rebudgeted = base.clone();
    rebudgeted.regions[0].max_tokens = 2_000;
    let mut recounted = base.clone();
    recounted.regions[0].current_tokens = 99;
    let mut whole_budget = base.clone();
    whole_budget.max_tokens = 20_000;
    let mut whole_total = base.clone();
    whole_total.total_tokens = 7;
    let rewritten = window(vec![region("conv", vec![entry("goodbye", 2)])]);
    let grown = window(vec![region(
        "conv",
        vec![entry("hello", 2), entry("again", 1)],
    )]);
    let reordered = window(vec![
        region("plan", vec![entry("p", 1)]),
        region("conv", vec![entry("hello", 2)]),
    ]);
    let other_order = window(vec![
        region("conv", vec![entry("hello", 2)]),
        region("plan", vec![entry("p", 1)]),
    ]);

    for (what, other) in [
        ("a renamed region", &renamed),
        ("a region of another kind", &rekinded),
        ("a raised region budget", &rebudgeted),
        ("a recounted region", &recounted),
        ("a raised window budget", &whole_budget),
        ("a recounted window", &whole_total),
        ("rewritten contents", &rewritten),
        ("an appended entry", &grown),
    ] {
        assert_ne!(revision, context_revision(other), "{what}");
    }
    assert_ne!(
        context_revision(&reordered),
        context_revision(&other_order),
        "layout order is part of the window, because it is the order the prompt \
         is assembled in"
    );
}

/// Every field of an entry is in its fingerprint. A revision that ignored one
/// would answer for content the window does not hold.
#[test]
fn every_field_of_an_entry_is_in_its_fingerprint() {
    let base = entry("text", 1);
    let plain = region_digest([EntryFacts::from(&base)]);
    let variants = [
        ("other text", entry("other", 1)),
        ("another token count", entry("text", 2)),
        (
            "a key",
            RegionEntrySnapshot {
                key: Some("k".to_string()),
                ..entry("text", 1)
            },
        ),
        (
            "metadata",
            RegionEntrySnapshot {
                metadata: Some(serde_json::json!({"a": 1})),
                ..entry("text", 1)
            },
        ),
        (
            "another kind",
            RegionEntrySnapshot {
                kind: EntryKind::ToolResult {
                    tool_call_id: "c1".to_string(),
                    tool_name: "shell".to_string(),
                    is_error: false,
                },
                ..entry("text", 1)
            },
        ),
        (
            "a taint level",
            RegionEntrySnapshot {
                taint: crate::taint::TaintLevel::Private,
                ..entry("text", 1)
            },
        ),
        (
            "a reasoning token",
            RegionEntrySnapshot {
                reasoning: Some("sealed".to_string()),
                ..entry("text", 1)
            },
        ),
    ];
    for (what, variant) in &variants {
        assert_ne!(
            plain,
            region_digest([EntryFacts::from(variant)]),
            "{what} changes the entry"
        );
    }
}

/// A region digest covers the entries and nothing else, so two regions holding
/// the same entries under different names and budgets share it. What tells them
/// apart is the window revision, which carries the name and the budget.
#[test]
fn a_region_digest_is_over_its_contents_alone() {
    let one = region("conv", vec![entry("hello", 2)]);
    let other = RegionSnapshot {
        name: "notes".to_string(),
        kind: "pinned".to_string(),
        max_tokens: 5,
        ..one.clone()
    };
    assert_eq!(snapshot_region_digest(&one), snapshot_region_digest(&other));
    assert!(snapshot_region_digest(&one).starts_with("rg1-"));
    // An empty region still has a digest, rather than nothing to compare.
    let empty = snapshot_region_digest(&region("conv", vec![]));
    assert!(empty.starts_with("rg1-"));
    assert_ne!(empty, snapshot_region_digest(&one));
}

/// Two windows whose fields concatenate to the same bytes are still two
/// windows. The length prefix is what keeps them apart, and without it the pair
/// below shares a revision.
#[test]
fn a_field_boundary_is_part_of_the_identity() {
    let first = window(vec![region("ab", vec![])]);
    let second = window(vec![region("a", vec![])]);
    let mut shifted = second.clone();
    shifted.regions[0].kind = "bclearable".to_string();
    assert_ne!(context_revision(&first), context_revision(&shifted));
}

/// An entry whose metadata was built key-by-key in a different order is the same
/// entry: the canonical form orders a map's keys, so a revision survives a
/// round trip through the journal.
#[test]
fn metadata_key_order_does_not_move_a_digest() {
    let forwards = RegionEntrySnapshot {
        metadata: Some(serde_json::json!({"a": 1, "b": 2})),
        ..entry("text", 1)
    };
    let backwards = RegionEntrySnapshot {
        metadata: Some(serde_json::json!({"b": 2, "a": 1})),
        ..entry("text", 1)
    };
    assert_eq!(
        region_digest([EntryFacts::from(&forwards)]),
        region_digest([EntryFacts::from(&backwards)])
    );
}

/// A window with no regions has a revision of its own rather than an empty
/// string: a run before its first write is a window, and naming it is how a
/// reader asks what the first change started from.
#[test]
fn an_empty_window_still_has_a_revision() {
    let empty = context_revision(&window(vec![]));
    assert!(empty.starts_with("cw1-"), "{empty}");
    assert_ne!(
        empty,
        context_revision(&window(vec![region("conv", vec![])])),
        "a window holding one empty region is not an empty window"
    );
}
