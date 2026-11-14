use super::*;
use leviath_core::{EntryKind, EvictionStrategy, RegionKind};

fn pinned(name: &str, entries: &[&str]) -> Region {
    let mut region = Region::new(name.to_string(), RegionKind::Pinned, 10_000);
    for entry in entries {
        region.add_entry(*entry, 2).unwrap();
    }
    region
}

fn turns(name: &str) -> Region {
    Region::new(
        name.to_string(),
        RegionKind::SlidingWindow {
            max_items: 3,
            eviction_strategy: EvictionStrategy::PerItem,
        },
        10_000,
    )
}

/// `regions` with a conversation holding `said`, user and assistant in turn.
fn with_talk(mut regions: Vec<Region>, said: &[&str]) -> Vec<Region> {
    let mut conversation = Region::new(CONVERSATION.to_string(), RegionKind::Pinned, 10_000);
    for (i, text) in said.iter().enumerate() {
        let kind = match i % 2 {
            0 => EntryKind::UserMessage,
            _ => EntryKind::AssistantTurn { tool_calls: vec![] },
        };
        conversation.add_typed_entry(*text, 2, kind).unwrap();
    }
    regions.push(conversation);
    regions
}

fn texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|m| format!("{}: {:?}", m.role, m.content))
        .collect()
}

/// What assembly would hand the frame for these regions.
fn live<'a>(stage: &'a str, iteration: usize, regions: &'a [Region]) -> Live<'a> {
    let blocks = leviath_core::context::assemble_system(regions, |_, _| {}).blocks;
    Live {
        stage,
        iteration,
        regions: regions.iter().collect(),
        blocks,
        head: Vec::new(),
    }
}

fn system_text(framed: &Framed) -> Vec<String> {
    framed
        .system_blocks
        .iter()
        .map(|b| b.text.clone())
        .collect()
}

/// The stage's first request shows the window as it is and marks nothing
/// it repeats.
#[test]
fn a_first_request_shows_the_window_as_it_is() {
    let regions = with_talk(vec![pinned("codebase", &["fn a() {}"])], &["go"]);
    let first = frame(None, live("s", 0, &regions));
    assert_eq!(system_text(&first), ["## codebase\nfn a() {}"]);
    assert_eq!(first.repeated, 0);
    assert_eq!(texts(&first.messages), ["user: Text(\"go\")"]);
    assert_eq!(first.frame.sent, 1);
}

/// What a region gains goes out as a turn after the messages that were there,
/// the system prompt stays as the first request sent it, and the next request
/// repeats all of this one.
#[test]
fn a_gain_is_sent_as_a_turn_and_the_system_prompt_holds() {
    let regions = with_talk(vec![pinned("codebase", &["fn a() {}"])], &["go"]);
    let first = frame(None, live("s", 0, &regions));

    let regions = with_talk(
        vec![pinned("codebase", &["fn a() {}", "fn b() {}"])],
        &["go", "read b"],
    );
    let second = frame(Some(&first.frame), live("s", 1, &regions));
    assert_eq!(system_text(&second), system_text(&first));
    assert_eq!(second.system_hash, first.system_hash);
    assert_eq!(second.repeated, 1);
    assert_eq!(
        texts(&second.messages),
        [
            "user: Text(\"go\")",
            "assistant: Text(\"read b\")",
            "user: Text(\"## codebase (continued)\\nfn b() {}\")",
        ]
    );

    // Nothing more gained: the turn stays where it was and no other is added.
    let regions = with_talk(
        vec![pinned("codebase", &["fn a() {}", "fn b() {}"])],
        &["go", "read b", "thanks"],
    );
    let third = frame(Some(&second.frame), live("s", 2, &regions));
    assert_eq!(third.repeated, 3);
    assert_eq!(texts(&third.messages)[..3], texts(&second.messages)[..]);
    assert_eq!(third.messages.len(), 4);
}

/// A region the first request did not show (it was empty) is sent under its
/// own heading, and one still empty adds nothing.
#[test]
fn a_region_first_written_during_the_stage_is_sent_under_its_own_heading() {
    let regions = with_talk(
        vec![pinned("codebase", &["a"]), pinned("notes", &[])],
        &["go"],
    );
    let first = frame(None, live("s", 0, &regions));
    let unchanged = frame(Some(&first.frame), live("s", 1, &regions));
    assert_eq!(unchanged.messages.len(), 1);

    let regions = with_talk(
        vec![
            pinned("codebase", &["a"]),
            pinned("notes", &["remember this"]),
        ],
        &["go"],
    );
    let second = frame(Some(&first.frame), live("s", 1, &regions));
    assert_eq!(
        texts(&second.messages)[1],
        "user: Text(\"## notes\\nremember this\")"
    );
}

/// A sliding window other than the conversation is sent as turns ahead of the
/// dialogue. What it gains during the stage goes after the history, so a
/// `context_append` to it never moves a turn the previous request sent.
#[test]
fn a_note_appended_to_a_sliding_window_never_moves_an_earlier_turn() {
    let mut decisions = turns("decisions");
    decisions
        .add_typed_entry("use serde", 2, EntryKind::Text)
        .unwrap();
    let regions = with_talk(vec![decisions.clone()], &["go"]);
    let first = frame(None, live("s", 0, &regions));
    assert_eq!(
        texts(&first.messages),
        ["user: Text(\"use serde\")", "user: Text(\"go\")"]
    );

    // Spelled with a speaker, as a writer may: in place that prefix is
    // dropped, and so it is here.
    decisions
        .add_typed_entry("User: keep the old parser", 2, EntryKind::Text)
        .unwrap();
    let regions = with_talk(vec![decisions.clone()], &["go", "noted"]);
    let second = frame(Some(&first.frame), live("s", 1, &regions));
    assert_eq!(second.system_hash, first.system_hash);
    assert_eq!(second.repeated, 2);
    assert_eq!(
        texts(&second.messages),
        [
            "user: Text(\"use serde\")",
            "user: Text(\"go\")",
            "assistant: Text(\"noted\")",
            "user: Text(\"keep the old parser\")",
        ]
    );

    // The dialogue goes on; the note stays where it was sent.
    let regions = with_talk(vec![decisions], &["go", "noted", "next"]);
    let third = frame(Some(&second.frame), live("s", 2, &regions));
    assert_eq!(third.repeated, 4);
    assert_eq!(texts(&third.messages)[..4], texts(&second.messages)[..]);
    assert_eq!(texts(&third.messages)[4], "user: Text(\"next\")");
}

/// A sliding window that evicted a turn, or gained a turn of the dialogue
/// rather than a note, cannot be continued: the window is shown afresh.
#[test]
fn a_sliding_window_that_moved_otherwise_shows_the_window_afresh() {
    let mut log = turns("changelog");
    for line in ["a", "b", "c"] {
        log.add_typed_entry(line, 2, EntryKind::Text).unwrap();
    }
    let regions = with_talk(vec![log.clone()], &["go"]);
    let first = frame(None, live("s", 0, &regions));

    // Over `max_items`: the oldest line goes.
    let mut evicted = log.clone();
    evicted.add_typed_entry("d", 2, EntryKind::Text).unwrap();
    assert_eq!(evicted.content.len(), 3);
    let regions = with_talk(vec![evicted], &["go"]);
    let afresh = frame(Some(&first.frame), live("s", 1, &regions));
    assert_eq!(afresh.repeated, 0);

    let mut log = turns("changelog");
    log.add_typed_entry("a", 2, EntryKind::Text).unwrap();
    let regions = with_talk(vec![log.clone()], &["go"]);
    let first = frame(None, live("s", 0, &regions));
    for spelled_as_a_reply in [
        ("done", EntryKind::AssistantTurn { tool_calls: vec![] }),
        ("Assistant: done", EntryKind::Text),
    ] {
        let mut replied = log.clone();
        replied
            .add_typed_entry(spelled_as_a_reply.0, 2, spelled_as_a_reply.1)
            .unwrap();
        let regions = with_talk(vec![replied], &["go"]);
        assert_eq!(
            frame(Some(&first.frame), live("s", 1, &regions)).repeated,
            0
        );
    }
}

/// Each way the window can move that a turn cannot say shows it afresh: the
/// system prompt is the window's own again and nothing is repeated.
#[test]
fn a_change_that_is_not_an_append_shows_the_window_afresh() {
    let regions = with_talk(vec![pinned("codebase", &["a", "b"])], &["go"]);
    let first = frame(None, live("s", 0, &regions));
    let afresh = |framed: &Framed, regions: &[Region]| {
        assert_eq!(framed.repeated, 0);
        let blocks = leviath_core::context::assemble_system(regions, |_, _| {}).blocks;
        assert_eq!(
            system_text(framed),
            blocks.iter().map(|b| b.text.clone()).collect::<Vec<_>>()
        );
    };

    // Another stage, and the first request of this one again.
    afresh(&frame(Some(&first.frame), live("t", 1, &regions)), &regions);
    afresh(&frame(Some(&first.frame), live("s", 0, &regions)), &regions);
    // A conversation that lost or rewrote a turn keeps the system prompt: see
    // `a_conversation_that_evicted_turns_keeps_the_system_prompt`.
    // An entry replaced, as a re-read of the same file does.
    let replaced = with_talk(vec![pinned("codebase", &["b", "a2"])], &["go"]);
    afresh(
        &frame(Some(&first.frame), live("s", 1, &replaced)),
        &replaced,
    );
    // A region emptied or no longer shown.
    let gone = with_talk(vec![pinned("other", &[])], &["go"]);
    afresh(&frame(Some(&first.frame), live("s", 1, &gone)), &gone);
    // A region whose gain cannot be said as text.
    let mut tasks = Region::new("codebase".to_string(), RegionKind::Checklist, 10_000);
    tasks.add_entry("a", 2).unwrap();
    let checklist = with_talk(vec![tasks.clone()], &["go"]);
    let shown = frame(None, live("s", 0, &checklist));
    tasks.add_entry("c", 2).unwrap();
    let grown = with_talk(vec![tasks], &["go"]);
    afresh(&frame(Some(&shown.frame), live("s", 1, &grown)), &grown);
}

/// The same entries rendered differently - a script's region, a checklist
/// item ticked - is a change too, and so is a block of no region's moving.
#[test]
fn a_rendering_that_moved_shows_the_window_afresh() {
    let regions = with_talk(vec![pinned("codebase", &["a"])], &["go"]);
    let first = frame(None, live("s", 0, &regions));

    let mut restyled = live("s", 1, &regions);
    restyled.blocks[0].text.push('!');
    let framed = frame(Some(&first.frame), restyled);
    assert_eq!(framed.repeated, 0);
    assert_eq!(system_text(&framed), ["## codebase\na!"]);

    let mut scripted = live("s", 1, &regions);
    scripted.blocks.push(SystemBlock {
        text: "<from a script>".to_string(),
        cache_hint: leviath_core::CacheHint::UntilChanged,
        region: String::new(),
        volatility: leviath_core::Volatility::default(),
    });
    assert_eq!(frame(Some(&first.frame), scripted).repeated, 0);
}

/// What leads the conversation (lifted parts, a script's messages) leads it in
/// every request, and a change to it is a change to the history.
#[test]
fn the_head_leads_and_holds() {
    let regions = with_talk(vec![pinned("codebase", &["a"])], &["go"]);
    let lead = |text: &str| Message {
        role: "user".to_string(),
        content: text.into(),
        cache_breakpoint: false,
        reasoning: None,
    };
    let mut first_live = live("s", 0, &regions);
    first_live.head = vec![lead("parts")];
    let first = frame(None, first_live);
    assert_eq!(
        texts(&first.messages),
        ["user: Text(\"parts\")", "user: Text(\"go\")"]
    );
    let mut same = live("s", 1, &regions);
    same.head = vec![lead("parts")];
    assert_eq!(frame(Some(&first.frame), same).repeated, 2);
    let mut moved = live("s", 1, &regions);
    moved.head = vec![lead("other parts")];
    assert_eq!(frame(Some(&first.frame), moved).repeated, 0);
}

/// An update made before there was any message still goes first, and each
/// lands after exactly the messages that came before it.
#[test]
fn updates_land_after_the_messages_before_them() {
    let say = |role: &str, text: &str| Message {
        role: role.to_string(),
        content: text.into(),
        cache_breakpoint: false,
        reasoning: None,
    };
    let update = |after: usize, text: &str| Update {
        after,
        turns: vec![say("user", text)],
    };
    let placed = place(Vec::new(), &[update(0, "u0")]);
    assert_eq!(texts(&placed), ["user: Text(\"u0\")"]);
    let placed = place(
        vec![say("user", "m0"), say("assistant", "m1")],
        &[update(0, "u0"), update(1, "u1"), update(2, "u2")],
    );
    assert_eq!(
        texts(&placed),
        [
            "user: Text(\"u0\")",
            "user: Text(\"m0\")",
            "user: Text(\"u1\")",
            "assistant: Text(\"m1\")",
            "user: Text(\"u2\")",
        ]
    );
}

/// A note sent after the history reads exactly as it would have in place:
/// the turns a window sends for its entries, not a block about them.
#[test]
fn a_late_note_reads_as_it_would_have_in_place() {
    let mut decisions = turns("decisions");
    decisions
        .add_typed_entry("one", 2, EntryKind::Text)
        .unwrap();
    let regions = with_talk(vec![decisions.clone()], &["go"]);
    let first = frame(None, live("s", 0, &regions));
    for note in ["two", "User: three"] {
        decisions.add_typed_entry(note, 2, EntryKind::Text).unwrap();
    }
    let late = with_talk(vec![decisions.clone()], &["go"]);
    let carried = frame(Some(&first.frame), live("s", 1, &late));
    let in_place = frame(None, live("s", 0, &late));
    let tail = |m: &[Message]| {
        texts(m)
            .into_iter()
            .filter(|t| t != "user: Text(\"go\")")
            .collect::<Vec<_>>()
    };
    assert_eq!(tail(&carried.messages), tail(&in_place.messages));
}

/// A call whose result has not landed is stripped before the frame counts or
/// places anything, so a turn added meanwhile goes ahead of the call, never
/// between it and the result that pairs with it a request later.
#[test]
fn a_turn_is_never_placed_between_a_call_and_its_result() {
    let call = |id: &str| leviath_core::SerializedToolCall {
        id: id.to_string(),
        name: "read_file".to_string(),
        arguments: serde_json::json!({}),
        thought_signature: None,
    };
    let with_call = |codebase: &[&str], answered: bool| {
        let mut regions = with_talk(vec![pinned("codebase", codebase)], &["go"]);
        let conversation = regions.last_mut().unwrap();
        conversation
            .add_typed_entry(
                "",
                1,
                EntryKind::AssistantTurn {
                    tool_calls: vec![call("c1")],
                },
            )
            .unwrap();
        if answered {
            conversation
                .add_typed_entry(
                    "[in codebase]",
                    2,
                    EntryKind::ToolResult {
                        tool_call_id: "c1".to_string(),
                        tool_name: "read_file".to_string(),
                        is_error: false,
                    },
                )
                .unwrap();
        }
        regions
    };
    let unanswered = with_call(&["a"], false);
    let first = frame(None, live("s", 0, &unanswered));
    assert_eq!(texts(&first.messages), ["user: Text(\"go\")"]);
    let grew = with_call(&["a", "b"], false);
    let second = frame(Some(&first.frame), live("s", 1, &grew));
    let answered = with_call(&["a", "b"], true);
    let third = frame(Some(&second.frame), live("s", 2, &answered));
    assert_eq!(third.repeated, second.messages.len());
    let roles: Vec<&str> = third.messages.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(roles, ["user", "user", "assistant", "user"]);
    assert!(texts(&third.messages)[1].contains("codebase (continued)"));
    assert!(matches!(
        &third.messages[3].content,
        leviath_providers::MessageContent::Blocks(b)
            if matches!(b[0], leviath_providers::ContentBlock::ToolResult { .. })
    ));
}

/// How many of `messages` say exactly `text`.
fn sent_times(messages: &[Message], text: &str) -> usize {
    texts(messages).iter().filter(|t| *t == text).count()
}

const GAINED_B: &str = "user: Text(\"## codebase (continued)\\nb\")";
const GAINED_C: &str = "user: Text(\"## codebase (continued)\\nc\")";
const GAINED_D: &str = "user: Text(\"## codebase (continued)\\nd\")";

/// The conversation evicted its oldest turns while `codebase` gained: the
/// system prompt is the one the last request sent, the update owed from
/// before moves to the front, the new gain follows the dialogue, nothing is
/// promised to repeat, and nothing is sent twice.
#[test]
fn a_conversation_that_evicted_turns_keeps_the_system_prompt() {
    let regions = with_talk(vec![pinned("codebase", &["a"])], &["u0", "a0"]);
    let first = frame(None, live("s", 0, &regions));
    let regions = with_talk(
        vec![pinned("codebase", &["a", "b"])],
        &["u0", "a0", "u1", "a1"],
    );
    let second = frame(Some(&first.frame), live("s", 1, &regions));
    assert_eq!(second.repeated, 2);

    let evicted = with_talk(
        vec![pinned("codebase", &["a", "b", "c"])],
        &["u1", "a1", "u2"],
    );
    let third = frame(Some(&second.frame), live("s", 2, &evicted));
    assert_eq!(system_text(&third), system_text(&first));
    assert_eq!(third.system_hash, first.system_hash);
    assert_eq!(third.repeated, 0);
    assert_eq!(
        texts(&third.messages),
        [
            GAINED_B,
            "user: Text(\"u1\")",
            "assistant: Text(\"a1\")",
            "user: Text(\"u2\")",
            GAINED_C,
        ]
    );
    for said in [GAINED_B, GAINED_C, "user: Text(\"u1\")"] {
        assert_eq!(sent_times(&third.messages, said), 1, "{said}");
    }
    assert_eq!(third.frame.sent, 5);
}

/// An eviction with no update owed keeps the system prompt and sends the
/// dialogue as the window holds it now.
#[test]
fn an_eviction_with_no_update_owed_sends_the_dialogue_as_it_is() {
    let regions = with_talk(vec![pinned("codebase", &["a"])], &["u0", "a0", "u1"]);
    let first = frame(None, live("s", 0, &regions));
    let evicted = with_talk(vec![pinned("codebase", &["a"])], &["u1", "a1", "u2"]);
    let second = frame(Some(&first.frame), live("s", 1, &evicted));
    assert_eq!(system_text(&second), system_text(&first));
    assert_eq!(second.system_hash, first.system_hash);
    assert_eq!(second.repeated, 0);
    assert_eq!(
        texts(&second.messages),
        texts(&frame(None, live("s", 0, &evicted)).messages)
    );
}

/// The request after a soft restart carries as any other: it repeats all the
/// soft restart sent and only appends.
#[test]
fn the_request_after_an_eviction_carries_normally() {
    let regions = with_talk(vec![pinned("codebase", &["a"])], &["u0", "a0"]);
    let first = frame(None, live("s", 0, &regions));
    let regions = with_talk(vec![pinned("codebase", &["a", "b"])], &["u0", "a0", "u1"]);
    let second = frame(Some(&first.frame), live("s", 1, &regions));
    let evicted = with_talk(vec![pinned("codebase", &["a", "b", "c"])], &["u1", "a1"]);
    let third = frame(Some(&second.frame), live("s", 2, &evicted));
    assert_eq!(third.repeated, 0);

    let regions = with_talk(
        vec![pinned("codebase", &["a", "b", "c", "d"])],
        &["u1", "a1", "u2"],
    );
    let fourth = frame(Some(&third.frame), live("s", 3, &regions));
    assert_eq!(fourth.system_hash, first.system_hash);
    assert_eq!(fourth.repeated, third.messages.len());
    assert_eq!(
        texts(&fourth.messages)[..third.messages.len()],
        texts(&third.messages)[..]
    );
    assert_eq!(
        texts(&fourth.messages)[third.messages.len()..],
        ["user: Text(\"u2\")", GAINED_D]
    );
}

/// Two evictions in one stage, the second on the very request after the
/// first: each keeps the system prompt, and every update owed goes to the
/// front in the order it was gained, once.
#[test]
fn two_evictions_in_one_stage_each_keep_the_system_prompt() {
    let regions = with_talk(vec![pinned("codebase", &["a"])], &["u0", "a0"]);
    let first = frame(None, live("s", 0, &regions));
    let regions = with_talk(
        vec![pinned("codebase", &["a", "b"])],
        &["u0", "a0", "u1", "a1"],
    );
    let second = frame(Some(&first.frame), live("s", 1, &regions));
    let evicted = with_talk(
        vec![pinned("codebase", &["a", "b", "c"])],
        &["u1", "a1", "u2", "a2"],
    );
    let third = frame(Some(&second.frame), live("s", 2, &evicted));
    let evicted_again = with_talk(
        vec![pinned("codebase", &["a", "b", "c", "d"])],
        &["u2", "a2", "u3"],
    );
    let fourth = frame(Some(&third.frame), live("s", 3, &evicted_again));
    assert_eq!(system_text(&fourth), system_text(&first));
    assert_eq!(fourth.system_hash, first.system_hash);
    assert_eq!(fourth.repeated, 0);
    assert_eq!(
        texts(&fourth.messages),
        [
            GAINED_B,
            GAINED_C,
            "user: Text(\"u2\")",
            "assistant: Text(\"a2\")",
            "user: Text(\"u3\")",
            GAINED_D,
        ]
    );
}

/// An eviction together with a region rewritten in place is a change a turn
/// cannot say: the window is shown afresh, as before.
#[test]
fn an_eviction_with_a_region_rewritten_shows_the_window_afresh() {
    let regions = with_talk(vec![pinned("codebase", &["a", "b"])], &["u0", "a0", "u1"]);
    let first = frame(None, live("s", 0, &regions));
    let moved = with_talk(vec![pinned("codebase", &["b", "a2"])], &["u1", "a1"]);
    let framed = frame(Some(&first.frame), live("s", 1, &moved));
    assert_eq!(framed.repeated, 0);
    assert_eq!(system_text(&framed), ["## codebase\nb\n\na2"]);
    assert_ne!(framed.system_hash, first.system_hash);
}

/// On a stage's first request, or under another stage, an eviction shows the
/// window afresh, its gains in place.
#[test]
fn an_eviction_on_a_first_request_or_another_stage_shows_afresh() {
    let regions = with_talk(vec![pinned("codebase", &["a"])], &["u0", "a0", "u1"]);
    let first = frame(None, live("s", 0, &regions));
    let evicted = with_talk(vec![pinned("codebase", &["a", "b"])], &["u1", "a1"]);
    for (stage, iteration) in [("s", 0), ("t", 1)] {
        let framed = frame(Some(&first.frame), live(stage, iteration, &evicted));
        assert_eq!(framed.repeated, 0);
        assert_eq!(system_text(&framed), ["## codebase\na\n\nb"]);
        assert_eq!(
            texts(&framed.messages),
            ["user: Text(\"u1\")", "assistant: Text(\"a1\")"]
        );
    }
}

/// A head that changed while the regions only gained keeps the system
/// prompt: the new head leads, and the gain follows the dialogue.
#[test]
fn a_changed_head_keeps_the_system_prompt() {
    let lead = |text: &str| Message {
        role: "user".to_string(),
        content: text.into(),
        cache_breakpoint: false,
        reasoning: None,
    };
    let regions = with_talk(vec![pinned("codebase", &["a"])], &["go"]);
    let mut first_live = live("s", 0, &regions);
    first_live.head = vec![lead("parts")];
    let first = frame(None, first_live);
    let regions = with_talk(vec![pinned("codebase", &["a", "b"])], &["go"]);
    let mut moved = live("s", 1, &regions);
    moved.head = vec![lead("other parts")];
    let framed = frame(Some(&first.frame), moved);
    assert_eq!(system_text(&framed), system_text(&first));
    assert_eq!(framed.system_hash, first.system_hash);
    assert_eq!(framed.repeated, 0);
    assert_eq!(
        texts(&framed.messages),
        [
            "user: Text(\"other parts\")",
            "user: Text(\"go\")",
            GAINED_B
        ]
    );
}

/// The conversation that survives an eviction can start with a result whose
/// call was evicted. That result is stripped before anything is placed, so
/// an update moved to the front lands ahead of the dialogue and never next to
/// a result with no call.
#[test]
fn an_update_moved_to_the_front_never_splits_a_call_from_its_result() {
    let call = leviath_core::SerializedToolCall {
        id: "c1".to_string(),
        name: "read_file".to_string(),
        arguments: serde_json::json!({}),
        thought_signature: None,
    };
    let result = EntryKind::ToolResult {
        tool_call_id: "c1".to_string(),
        tool_name: "read_file".to_string(),
        is_error: false,
    };
    let conversation = |entries: &[(&str, EntryKind)]| {
        let mut region = Region::new(CONVERSATION.to_string(), RegionKind::Pinned, 10_000);
        for (text, kind) in entries {
            region.add_typed_entry(*text, 2, kind.clone()).unwrap();
        }
        region
    };
    let asked = EntryKind::AssistantTurn {
        tool_calls: vec![call],
    };
    let replied = EntryKind::AssistantTurn { tool_calls: vec![] };
    let early = [
        ("go", EntryKind::UserMessage),
        ("", asked),
        ("[in codebase]", result.clone()),
        ("ok", replied.clone()),
    ];
    let regions = vec![pinned("codebase", &["a"]), conversation(&early)];
    let first = frame(None, live("s", 0, &regions));
    let mut grown = early.to_vec();
    grown.push(("more", EntryKind::UserMessage));
    let regions = vec![pinned("codebase", &["a", "b"]), conversation(&grown)];
    let second = frame(Some(&first.frame), live("s", 1, &regions));

    // The user's turn and the call are evicted; the result survives.
    let survived = [
        ("[in codebase]", result),
        ("ok", replied.clone()),
        ("more", EntryKind::UserMessage),
        ("done", replied),
    ];
    let regions = vec![
        pinned("codebase", &["a", "b", "c"]),
        conversation(&survived),
    ];
    let third = frame(Some(&second.frame), live("s", 2, &regions));
    assert_eq!(third.system_hash, first.system_hash);
    assert_eq!(third.repeated, 0);
    assert_eq!(
        texts(&third.messages),
        [
            GAINED_B,
            "assistant: Text(\"ok\")",
            "user: Text(\"more\")",
            "assistant: Text(\"done\")",
            GAINED_C,
        ]
    );
}
