//! Tests for [`super`]: per-kind rendering, chunking, ordering, and the
//! cache-breakpoint decisions.

use super::*;
use crate::mime::{Blob, MimeRegistry, MimeType, Part};

/// A system block carrying only the hint under test.
fn block(hint: CacheHint) -> SystemBlock {
    SystemBlock {
        text: "x".to_string(),
        cache_hint: hint,
        volatility: Volatility::default(),
        region: String::new(),
    }
}

/// The number of cache breakpoints the Anthropic provider would place on
/// these system blocks: one per contiguous run of same-hint cacheable blocks.
fn provider_system_breakpoints(blocks: &[SystemBlock]) -> usize {
    let mut runs = 0;
    for (index, block) in blocks.iter().enumerate() {
        let hint = block.cache_hint;
        if hint != CacheHint::Never && blocks.get(index + 1).map(|b| b.cache_hint) != Some(hint) {
            runs += 1;
        }
    }
    runs
}

fn entries(n: usize, each: &str) -> Vec<String> {
    (0..n).map(|i| format!("{i}:{each}")).collect()
}

// ─── lifecycle kinds (issue #490) ───────────────────────────────────────

fn lifecycle_region(kind: RegionKind, volatility: Volatility, entry_count: usize) -> Region {
    let mut region = Region::new("corpus".to_string(), kind, 1_000_000);
    region.volatility = volatility;
    for i in 0..entry_count {
        let body = format!("excerpt {i} {}", "word ".repeat(600));
        region.add_entry(body, 750).unwrap();
    }
    region
}

/// A region that says nothing about how it moves keeps exactly the
/// behaviour it had: one block, uncacheable. This is what makes the change
/// safe to ship on by default - nobody who has not opted in is affected.
#[test]
fn an_undeclared_lifecycle_region_is_unchanged() {
    for kind in [RegionKind::Temporary, RegionKind::Clearable] {
        let region = lifecycle_region(kind, Volatility::Rewritten, 8);
        let mut blocks = Vec::new();
        push_bracketed(
            &mut blocks,
            &region,
            lifecycle_cache_hint(region.volatility),
        );

        assert_eq!(blocks.len(), 1, "one block, as before");
        assert_eq!(blocks[0].cache_hint, CacheHint::Never);
        assert!(blocks[0].text.starts_with("[corpus]:\n"));
    }
}

/// The fix. A corpus the author says accumulates gets interior boundaries
/// and a hint a marker can land on, so the settled head caches and only the
/// tail is re-sent.
#[test]
fn a_lifecycle_region_declared_grows_is_chunk_cacheable() {
    let region = lifecycle_region(RegionKind::Temporary, Volatility::Grows, 12);
    let mut blocks = Vec::new();
    push_bracketed(
        &mut blocks,
        &region,
        lifecycle_cache_hint(region.volatility),
    );

    let count = blocks.len();
    assert!(
        count > 1,
        "a 12-entry corpus past the chunk budget splits: {count} blocks"
    );
    assert!(
        blocks
            .iter()
            .all(|b| b.cache_hint == CacheHint::UntilChanged)
    );
    assert!(blocks[0].text.starts_with("[corpus]:\n"));
    assert!(
        blocks[1].text.starts_with("[corpus continued]:\n"),
        "a split region still says which region it is"
    );
}

/// Chunking must not change a single byte of what the model reads, headings
/// aside - the whole point is a cheaper way to send the same corpus.
#[test]
fn chunking_a_lifecycle_region_preserves_its_content() {
    let grows = lifecycle_region(RegionKind::Temporary, Volatility::Grows, 9);
    let whole = lifecycle_region(RegionKind::Temporary, Volatility::Rewritten, 9);
    let mut split = Vec::new();
    push_bracketed(&mut split, &grows, CacheHint::UntilChanged);
    let mut one = Vec::new();
    push_bracketed(&mut one, &whole, CacheHint::Never);

    let strip = |text: &str| {
        text.replace("[corpus continued]:\n", "")
            .replace("[corpus]:\n", "")
    };
    let rejoined: String = split
        .iter()
        .map(|b| strip(&b.text))
        .collect::<Vec<_>>()
        .join("\n\n");
    assert_eq!(rejoined, strip(&one[0].text));
}

/// The declaration is what decides, because the kind cannot: `temporary`
/// says when the region is thrown away, not whether it holds still between
/// those moments.
#[test]
fn the_hint_follows_the_declaration_not_the_kind() {
    use Volatility;
    assert_eq!(
        lifecycle_cache_hint(Volatility::Rewritten),
        CacheHint::Never
    );
    assert_eq!(
        lifecycle_cache_hint(Volatility::Grows),
        CacheHint::UntilChanged
    );
    assert_eq!(
        lifecycle_cache_hint(Volatility::Stable),
        CacheHint::UntilChanged
    );
}

// ─── chunking ───────────────────────────────────────────────────────────

/// The property the whole fix rests on: a boundary that existed last turn
/// exists this turn, with the same bytes in front of it. Without it there is
/// nothing a provider can match.
#[test]
fn appending_never_repacks_an_earlier_chunk() {
    let big = "word ".repeat(400);
    let mut before: Vec<String> = Vec::new();
    for turn in 1..12 {
        let now = append_only_chunks(&entries(turn, &big), 2048);
        // Every *full* chunk the previous turn had, this turn still has,
        // verbatim. The last chunk is the live tail and is meant to grow -
        // it is the only part a provider has to re-send.
        let frozen = before.len().saturating_sub(1);
        for (old, new) in before.iter().take(frozen).zip(&now) {
            assert_eq!(old, new, "a frozen chunk was repacked at turn {turn}");
        }
        assert!(now.len() >= before.len(), "chunks never merge back");
        before = now;
    }
}

/// Nothing is lost or reordered by splitting: the chunks rejoin into exactly
/// the text one unsplit block carries.
#[test]
fn chunking_preserves_the_content_exactly() {
    let all = entries(25, &"word ".repeat(120));
    let rejoined = append_only_chunks(&all, 2048).join("\n\n");
    assert_eq!(rejoined, all.join("\n\n"));
}

#[test]
fn a_region_with_nothing_in_it_produces_no_chunks() {
    assert!(append_only_chunks(&[], 2048).is_empty());
}

/// One entry larger than the budget is still one chunk: the entry is the
/// smallest thing there is to split on.
#[test]
fn an_entry_larger_than_the_budget_is_its_own_chunk() {
    let huge = "word ".repeat(5000);
    let chunks = append_only_chunks(std::slice::from_ref(&huge), 2048);
    assert_eq!(chunks, vec![huge.clone()]);
}

/// Splitting for the cache must not cost the model the heading. Every chunk
/// after the first says which region it continues.
#[test]
fn every_chunk_after_the_first_names_the_region_it_continues() {
    let mut region = Region::new("sources".to_string(), RegionKind::Pinned, 1_000_000);
    // Chunking is what a *growing* region gets; a region that does not say
    // so is left as one block, so the declaration is part of the fixture.
    region.volatility = Volatility::Grows;
    for entry in entries(20, &"word ".repeat(200)) {
        region.add_entry(entry, 250).expect("fits");
    }
    let contents: Vec<String> = region
        .content
        .iter()
        .map(|e| e.content.to_string())
        .collect();

    let mut blocks = Vec::new();
    push_chunked(&mut blocks, &region, &contents, CacheHint::Always);

    assert!(blocks.len() > 1, "the fixture is meant to span chunks");
    assert!(blocks[0].text.starts_with("## sources"));
    for block in &blocks[1..] {
        assert!(
            block.text.starts_with("## sources (continued)"),
            "a continuation arrived unlabelled: {:.40}",
            block.text
        );
    }
    // And every entry is still present, in order.
    let whole = blocks
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut last = 0;
    for (i, _) in contents.iter().enumerate() {
        let needle = format!("{i}:");
        let at = whole.find(&needle).expect("entry present");
        assert!(at >= last, "entries came out of order");
        last = at;
    }
}

// ─── the volatile-tier split ────────────────────────────────────────────────

#[test]
fn recently_changed_sorts_with_until_changed() {
    assert_eq!(
        cache_hint_sort_priority(CacheHint::RecentlyChanged),
        cache_hint_sort_priority(CacheHint::UntilChanged)
    );
    assert_eq!(cache_hint_sort_priority(CacheHint::RecentlyChanged), 2);
}

#[test]
fn mark_recently_changed_run_splits_at_the_newest_block() {
    // Always, three volatile blocks, and a trailing uncacheable block. The
    // third volatile block is the one that just changed.
    let mut blocks = vec![
        block(CacheHint::Always),
        block(CacheHint::UntilChanged),
        block(CacheHint::UntilChanged),
        block(CacheHint::UntilChanged),
        block(CacheHint::Never),
    ];
    mark_recently_changed_run(&mut blocks, &[10, 20, 90]);

    let hints: Vec<CacheHint> = blocks.iter().map(|b| b.cache_hint).collect();
    assert_eq!(
        hints,
        vec![
            CacheHint::Always,
            CacheHint::UntilChanged,
            CacheHint::UntilChanged,
            CacheHint::RecentlyChanged,
            CacheHint::Never,
        ]
    );
    // Always run, stable volatile head, changed volatile tail. The
    // uncacheable trailing block claims nothing.
    assert_eq!(provider_system_breakpoints(&blocks), 3);
}

#[test]
fn mark_recently_changed_run_retags_every_block_after_the_boundary() {
    let mut blocks = vec![
        block(CacheHint::UntilChanged),
        block(CacheHint::UntilChanged),
        block(CacheHint::UntilChanged),
    ];
    mark_recently_changed_run(&mut blocks, &[1, 7, 5]);

    let hints: Vec<CacheHint> = blocks.iter().map(|b| b.cache_hint).collect();
    assert_eq!(
        hints,
        vec![
            CacheHint::UntilChanged,
            CacheHint::RecentlyChanged,
            CacheHint::RecentlyChanged,
        ]
    );
}

#[test]
fn mark_recently_changed_run_ties_resolve_to_the_earliest_block() {
    // Two regions written in the same second both changed, so the boundary
    // belongs ahead of the earlier one.
    let mut blocks = vec![
        block(CacheHint::UntilChanged),
        block(CacheHint::UntilChanged),
        block(CacheHint::UntilChanged),
    ];
    mark_recently_changed_run(&mut blocks, &[1, 9, 9]);

    assert_eq!(blocks[0].cache_hint, CacheHint::UntilChanged);
    assert_eq!(blocks[1].cache_hint, CacheHint::RecentlyChanged);
    assert_eq!(blocks[2].cache_hint, CacheHint::RecentlyChanged);
}

#[test]
fn mark_recently_changed_run_leaves_a_headless_tier_alone() {
    // The newest block is already first, so there is no stable head to put
    // behind a breakpoint.
    let mut blocks = vec![
        block(CacheHint::UntilChanged),
        block(CacheHint::UntilChanged),
    ];
    mark_recently_changed_run(&mut blocks, &[42, 1]);
    assert!(
        blocks
            .iter()
            .all(|b| b.cache_hint == CacheHint::UntilChanged)
    );
}

#[test]
fn mark_recently_changed_run_leaves_a_tierless_prompt_alone() {
    // No volatile blocks at all means an empty recency list.
    let mut blocks = vec![block(CacheHint::Always), block(CacheHint::Never)];
    mark_recently_changed_run(&mut blocks, &[]);
    assert_eq!(blocks[0].cache_hint, CacheHint::Always);
    assert_eq!(blocks[1].cache_hint, CacheHint::Never);
}

#[test]
fn mark_recently_changed_run_refuses_when_the_run_budget_is_full() {
    // Always, SlidingPrefix and UntilChanged already claim three
    // breakpoints. Splitting further would take the messages' one.
    let mut blocks = vec![
        block(CacheHint::Always),
        block(CacheHint::SlidingPrefix),
        block(CacheHint::UntilChanged),
        block(CacheHint::UntilChanged),
    ];
    assert_eq!(provider_system_breakpoints(&blocks), 3);

    mark_recently_changed_run(&mut blocks, &[1, 99]);

    assert_eq!(blocks[2].cache_hint, CacheHint::UntilChanged);
    assert_eq!(blocks[3].cache_hint, CacheHint::UntilChanged);
    assert_eq!(provider_system_breakpoints(&blocks), 3);
}

#[test]
fn cache_hint_sort_priority_orders_by_stability() {
    // Most stable first (lowest priority), volatile last.
    assert_eq!(cache_hint_sort_priority(CacheHint::Always), 0);
    assert_eq!(cache_hint_sort_priority(CacheHint::SlidingPrefix), 1);
    assert_eq!(cache_hint_sort_priority(CacheHint::UntilChanged), 2);
    assert_eq!(cache_hint_sort_priority(CacheHint::Never), 3);
    // The four priorities are strictly increasing by volatility.
    assert!(
        cache_hint_sort_priority(CacheHint::Always)
            < cache_hint_sort_priority(CacheHint::SlidingPrefix)
    );
}

// ─── labels and parts ───────────────────────────────────────────────────────

fn stored(name: &str) -> Part {
    let reg = MimeRegistry::builtin();
    let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![1, 2, 3]).named(name);
    Part::stored(blob.describe(&reg)).named(name)
}

fn stand_in(part: &Part) -> String {
    part.blob().unwrap().stand_in.clone()
}

/// The name always; the description only when the blueprint opted in.
#[test]
fn a_label_carries_the_description_only_when_asked() {
    let mut region = Region::new("sources".into(), RegionKind::Pinned, 100);
    assert_eq!(labelled(&region, "body"), "## sources\nbody");
    region.description = Some("Cite as [n].".into());
    assert_eq!(labelled(&region, "body"), "## sources\nbody");
    region.describe_in_prompt = true;
    assert_eq!(
        labelled(&region, "body"),
        "## sources\nCite as [n].\n\nbody"
    );
}

/// Inline text as itself, an empty one dropped, a stored part after its
/// stand-in so a request that drops the bytes still names them.
#[test]
fn content_blocks_put_each_stored_part_after_its_stand_in() {
    let picture = stored("a.png");
    let content =
        EntryContent::from_parts(vec![Part::text("see"), Part::text(""), picture.clone()]);
    let blocks = content_blocks(&content);
    assert_eq!(
        blocks,
        vec![
            PartBlock::Text("see".into()),
            PartBlock::Text(stand_in(&picture)),
            PartBlock::Stored(&content.parts()[2]),
        ]
    );
}

/// Only stored parts are lifted, each after a pointer naming the region and,
/// when the entry has one, its key.
#[test]
fn lifted_blocks_point_at_the_region_and_key() {
    let mut region = Region::new("art".into(), RegionKind::Pinned, 10_000);
    region
        .add_entry(EntryContent::from_parts(vec![stored("a.png")]), 5)
        .unwrap();
    region
        .add_keyed_entry(
            "hero",
            EntryContent::from_parts(vec![Part::text("cap"), stored("b.png")]),
            5,
        )
        .unwrap();
    region.add_entry("just text", 1).unwrap();
    let blocks = lifted_blocks(&region);
    assert_eq!(blocks.len(), 4);
    assert_eq!(
        blocks[0],
        PartBlock::Text(format!("[art] {}", stand_in(&stored("a.png"))))
    );
    assert!(matches!(blocks[1], PartBlock::Stored(p) if p.is_named("a.png")));
    assert_eq!(
        blocks[2],
        PartBlock::Text(format!("[art / hero] {}", stand_in(&stored("b.png"))))
    );
    assert!(matches!(blocks[3], PartBlock::Stored(p) if p.is_named("b.png")));
}

// ─── per-kind rendering ─────────────────────────────────────────────────────

fn filled(name: &str, kind: RegionKind) -> Region {
    let mut region = Region::new(name.into(), kind, 10_000);
    region.add_entry(format!("{name} one"), 5).unwrap();
    region.add_entry(format!("{name} two"), 5).unwrap();
    region
}

fn rendered(region: &Region) -> Vec<SystemBlock> {
    let mut blocks = Vec::new();
    push_region_blocks(&mut blocks, region);
    blocks
}

/// Each kind that renders without a script gets the label and hint it has
/// always had.
#[test]
fn each_system_kind_renders_with_its_own_label_and_hint() {
    let pinned = rendered(&filled("task", RegionKind::Pinned));
    assert_eq!(pinned.len(), 1);
    assert_eq!(pinned[0].text, "## task\ntask one\n\ntask two");
    assert_eq!(pinned[0].cache_hint, CacheHint::Always);
    assert_eq!(pinned[0].region, "task");

    let history = rendered(&filled(
        "past",
        RegionKind::CompactHistory {
            source_region: "conversation".into(),
        },
    ));
    assert_eq!(history[0].text, "## past\npast one\n\npast two");
    assert_eq!(history[0].cache_hint, CacheHint::Always);

    let compacting = rendered(&filled(
        "notes",
        RegionKind::Compacting {
            threshold_tokens: 100,
        },
    ));
    assert_eq!(compacting[0].text, "[notes]:\nnotes one\n\nnotes two");
    assert_eq!(compacting[0].cache_hint, CacheHint::UntilChanged);

    for kind in [RegionKind::Temporary, RegionKind::Clearable] {
        let blocks = rendered(&filled("scratch", kind));
        assert_eq!(blocks[0].text, "[scratch]:\nscratch one\n\nscratch two");
        assert_eq!(blocks[0].cache_hint, CacheHint::Never);
    }
}

/// A hash map names each keyed entry by its key; an unkeyed one reads as
/// itself.
#[test]
fn a_hash_map_region_heads_each_entry_with_its_key() {
    let mut region = Region::new(
        "facts".into(),
        RegionKind::HashMap {
            max_entries: Some(8),
        },
        10_000,
    );
    region.add_keyed_entry("city", "Paris", 2).unwrap();
    region.add_entry("loose", 2).unwrap();
    let blocks = rendered(&region);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].text, "[facts]:\n### [city]\nParis\n\nloose");
    assert_eq!(blocks[0].cache_hint, CacheHint::UntilChanged);
}

/// A checklist renders its derived state, and nothing when it holds no items.
#[test]
fn a_checklist_renders_its_items_or_nothing() {
    let mut region = Region::new("todo".into(), RegionKind::Checklist, 10_000);
    region.add_entry("not an item", 2).unwrap();
    assert!(rendered(&region).is_empty());

    region.add_checklist_item("write it".into(), 2).unwrap();
    let blocks = rendered(&region);
    assert_eq!(blocks.len(), 1);
    assert!(
        blocks[0]
            .text
            .starts_with("## todo\nChecklist (1 open, 0 done):")
    );
    assert_eq!(blocks[0].cache_hint, CacheHint::UntilChanged);
}

/// The conversation and custom regions are the caller's to render.
#[test]
fn the_conversation_and_custom_regions_render_nothing_here() {
    let sliding = filled(
        "conversation",
        RegionKind::SlidingWindow {
            max_items: 10,
            eviction_strategy: crate::EvictionStrategy::PerItem,
        },
    );
    let custom = filled(
        "script",
        RegionKind::Custom {
            script: "r.rhai".into(),
            pinned: false,
        },
    );
    assert!(rendered(&sliding).is_empty());
    assert!(rendered(&custom).is_empty());
}

/// The conversation is the dialogue whatever kind it was declared: a
/// `compacting` one holding a turn that called a tool, and the pointer its
/// routed result left, is sent as that turn and that result, never as a
/// system block that keeps the result and loses the call. Any other
/// non-sliding region still renders into the system prompt.
#[test]
fn the_conversation_renders_as_turns_whatever_its_kind() {
    use crate::region::{EntryKind, SerializedToolCall};
    let mut conversation = Region::new(
        crate::tool_routing::CONVERSATION.into(),
        RegionKind::Compacting {
            threshold_tokens: 1_000,
        },
        10_000,
    );
    let call = SerializedToolCall {
        id: "c1".into(),
        name: "list_dir".into(),
        arguments: serde_json::json!({ "path": "." }),
        thought_signature: None,
    };
    conversation
        .add_typed_entry(
            "",
            1,
            EntryKind::AssistantTurn {
                tool_calls: vec![call],
            },
        )
        .unwrap();
    conversation
        .add_typed_entry(
            "[output is in your context under the 'files' heading]",
            9,
            EntryKind::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "list_dir".into(),
                is_error: false,
            },
        )
        .unwrap();
    let notes = filled(
        "notes",
        RegionKind::Compacting {
            threshold_tokens: 1_000,
        },
    );
    assert!(renders_as_turns(&conversation));
    assert!(!renders_as_turns(&notes));

    let assembled = assemble_context([&notes, &conversation], |_, _| {});
    let order: Vec<&str> = assembled
        .system
        .blocks
        .iter()
        .map(|b| b.region.as_str())
        .collect();
    assert_eq!(order, ["notes"]);
    let roles: Vec<conversation::Role> = assembled.conversation.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [conversation::Role::Assistant, conversation::Role::User]
    );
    let conversation::ConversationContent::Blocks(turn) = &assembled.conversation[0].content else {
        panic!("a turn with a call is pieces");
    };
    assert!(
        turn.iter().any(
            |b| matches!(b, conversation::ConversationBlock::ToolUse(c) if c.name == "list_dir")
        )
    );

    // A custom region keeps its script, whatever its name.
    let custom = Region::new(
        crate::tool_routing::CONVERSATION.into(),
        RegionKind::Custom {
            script: "c.rhai".into(),
            pinned: false,
        },
        100,
    );
    let mut handed = 0;
    let assembled = assemble_context([&custom], |_, _| handed += 1);
    assert_eq!(handed, 1);
    assert!(assembled.conversation.is_empty());
}

// ─── the whole system side ──────────────────────────────────────────────────

/// Empty regions are skipped, except a custom one, which may render
/// scaffolding; the conversation and custom regions go to the caller; stored
/// parts outside the conversation are lifted; blocks sort most-stable first.
#[test]
fn assemble_system_renders_sorts_and_hands_back_what_it_cannot_render() {
    let mut scratch = filled("scratch", RegionKind::Temporary);
    scratch
        .add_entry(EntryContent::from_parts(vec![stored("s.png")]), 5)
        .unwrap();
    let mut conversation = filled(
        "conversation",
        RegionKind::SlidingWindow {
            max_items: 10,
            eviction_strategy: crate::EvictionStrategy::PerItem,
        },
    );
    conversation
        .add_entry(EntryContent::from_parts(vec![stored("c.png")]), 5)
        .unwrap();
    let empty_pinned = Region::new("unused".into(), RegionKind::Pinned, 100);
    let empty_custom = Region::new(
        "frame".into(),
        RegionKind::Custom {
            script: "frame.rhai".into(),
            pinned: true,
        },
        100,
    );
    let task = filled("task", RegionKind::Pinned);
    let regions = [scratch, conversation, empty_pinned, empty_custom, task];

    let mut handed_back = Vec::new();
    let system = assemble_system(&regions, |region, blocks| {
        handed_back.push(region.name.clone());
        if region.name == "frame" {
            blocks.push(SystemBlock {
                text: "[frame]".into(),
                cache_hint: CacheHint::Always,
                volatility: Volatility::Stable,
                region: "frame".into(),
            });
        }
    });

    assert_eq!(handed_back, ["conversation", "frame"]);
    let order: Vec<&str> = system.blocks.iter().map(|b| b.region.as_str()).collect();
    // Stable first, then the rewritten tier by hint: always before never.
    assert_eq!(order, ["frame", "task", "scratch"]);
    // Only the scratch region's picture: the conversation's stays inline.
    assert_eq!(system.lifted.len(), 2);
    assert!(matches!(system.lifted[1], PartBlock::Stored(p) if p.is_named("s.png")));
    assert_eq!(
        system.block_hashes,
        system
            .blocks
            .iter()
            .map(|b| block_hash(&b.text))
            .collect::<Vec<_>>()
    );
    assert_eq!(system.system_hash, system_prefix_hash(&system.blocks));
}

/// The volatile tier is split at the region that changed last, and only the
/// hint moves.
#[test]
fn assemble_system_splits_the_volatile_tier_at_the_newest_region() {
    let stamped = |name: &str, at: i64| {
        let mut region = Region::new(
            name.to_string(),
            RegionKind::HashMap { max_entries: None },
            1_000,
        );
        region.add_entry(format!("{name} contents"), 5).unwrap();
        region.content[0].timestamp = at;
        region
    };
    let regions = [stamped("a", 10), stamped("b", 90), stamped("c", 20)];
    let system = assemble_system(&regions, |_, _| {});
    let hints: Vec<CacheHint> = system.blocks.iter().map(|b| b.cache_hint).collect();
    assert_eq!(
        hints,
        [
            CacheHint::UntilChanged,
            CacheHint::RecentlyChanged,
            CacheHint::RecentlyChanged
        ]
    );
    let order: Vec<&str> = system.blocks.iter().map(|b| b.region.as_str()).collect();
    assert_eq!(order, ["a", "b", "c"]);
}

/// Nothing in, nothing out: no blocks, nothing lifted.
#[test]
fn assemble_system_of_nothing_is_empty() {
    let system = assemble_system(std::iter::empty::<&Region>(), |_, _| {});
    assert!(system.blocks.is_empty() && system.lifted.is_empty());
    assert!(system.block_hashes.is_empty());
}

// ─── ordering and the message breakpoint ────────────────────────────────────

/// Volatility leads the sort; the hint only breaks ties inside a tier.
#[test]
fn volatility_outranks_the_cache_hint() {
    let at = |volatility, hint| SystemBlock {
        text: String::new(),
        cache_hint: hint,
        volatility,
        region: String::new(),
    };
    assert_eq!(
        block_sort_priority(&at(Volatility::Stable, CacheHint::Never)),
        (0, 3)
    );
    assert_eq!(
        block_sort_priority(&at(Volatility::Grows, CacheHint::Always)),
        (1, 0)
    );
    assert_eq!(
        block_sort_priority(&at(Volatility::Rewritten, CacheHint::Always)),
        (2, 0)
    );
}

/// Fourth from last on a long conversation, the first message on a short
/// one, nothing on a single message or when the prefix moved.
#[test]
fn the_message_breakpoint_sits_where_it_can_be_read_back() {
    assert_eq!(message_breakpoint(9, false), Some(5));
    assert_eq!(message_breakpoint(5, false), Some(1));
    assert_eq!(message_breakpoint(4, false), Some(0));
    assert_eq!(message_breakpoint(2, false), Some(0));
    assert_eq!(message_breakpoint(1, false), None);
    assert_eq!(message_breakpoint(0, false), None);
    assert_eq!(message_breakpoint(9, true), None);
}

/// A block serialized before `region` and `volatility` existed still reads.
#[test]
fn a_system_block_from_before_its_newer_fields_still_deserializes() {
    let block: SystemBlock = serde_json::from_str(r#"{"text":"t","cache_hint":"Always"}"#).unwrap();
    assert_eq!(block.region, "");
    assert_eq!(block.volatility, Volatility::default());
}

#[test]
fn only_an_always_cached_block_with_no_region_is_a_hint() {
    assert!(block(CacheHint::Always).is_hint());
    // The note about tools the stage does not offer: no region, but it moves.
    assert!(!block(CacheHint::UntilChanged).is_hint());
    let mut region = block(CacheHint::Always);
    region.region = "task".to_string();
    assert!(!region.is_hint());
}
