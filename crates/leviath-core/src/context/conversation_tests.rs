use super::*;
use crate::mime::{Blob, MimeRegistry, MimeType};
use crate::{EvictionStrategy, RegionKind};

fn history() -> Region {
    Region::new(
        "conversation".into(),
        RegionKind::SlidingWindow {
            max_items: 100,
            eviction_strategy: EvictionStrategy::PerItem,
        },
        100_000,
    )
}

fn stored(name: &str) -> Part {
    let reg = MimeRegistry::builtin();
    let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![1, 2, 3]).named(name);
    Part::stored(blob.describe(&reg)).named(name)
}

fn call(id: &str) -> SerializedToolCall {
    SerializedToolCall {
        id: id.into(),
        name: "read_file".into(),
        arguments: serde_json::json!({ "path": "a" }),
        thought_signature: None,
    }
}

fn result(id: &str, is_error: bool) -> EntryKind {
    EntryKind::ToolResult {
        tool_call_id: id.into(),
        tool_name: "read_file".into(),
        is_error,
    }
}

#[test]
fn roles_are_spelled_the_way_chat_apis_spell_them() {
    assert_eq!(Role::User.as_str(), "user");
    assert_eq!(Role::Assistant.as_str(), "assistant");
}

#[test]
fn tool_results_in_a_row_share_one_user_turn() {
    let mut region = history();
    region
        .add_typed_entry("go", 1, EntryKind::UserMessage)
        .unwrap();
    region
        .add_typed_entry_with_reasoning(
            "reading",
            1,
            EntryKind::AssistantTurn {
                tool_calls: vec![call("c1"), call("c2")],
            },
            Some("sealed".into()),
        )
        .unwrap();
    region
        .add_typed_entry("one", 1, result("c1", false))
        .unwrap();
    region
        .add_typed_entry(
            EntryContent::from_parts(vec![Part::text("two"), stored("p.png")]),
            1,
            result("c2", true),
        )
        .unwrap();
    region
        .add_typed_entry("thanks", 1, EntryKind::UserMessage)
        .unwrap();

    let messages = conversation_messages(&region);
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].content, ConversationContent::Text("go".into()));
    let calls = [call("c1"), call("c2")];
    assert_eq!(
        messages[1],
        ConversationMessage {
            role: Role::Assistant,
            content: ConversationContent::Blocks(vec![
                ConversationBlock::Text("reading".into()),
                ConversationBlock::ToolUse(&calls[0]),
                ConversationBlock::ToolUse(&calls[1]),
            ]),
            reasoning: Some("sealed".into()),
        }
    );
    let ConversationContent::Blocks(results) = &messages[2].content else {
        panic!("tool results are blocks");
    };
    assert_eq!(messages[2].role, Role::User);
    assert_eq!(
        results[0],
        ConversationBlock::ToolResult {
            tool_use_id: "c1".into(),
            content: "one".into(),
            is_error: false,
        }
    );
    assert!(matches!(
        &results[1],
        ConversationBlock::ToolResult { is_error: true, .. }
    ));
    assert!(matches!(results[2], ConversationBlock::Stored(_)));
    assert_eq!(results.len(), 3);
    assert_eq!(
        messages[3].content,
        ConversationContent::Text("thanks".into())
    );
}

#[test]
fn a_conversation_ending_on_tool_results_closes_with_them() {
    let mut region = history();
    region
        .add_typed_entry(
            "",
            1,
            EntryKind::AssistantTurn {
                tool_calls: vec![call("c1")],
            },
        )
        .unwrap();
    region
        .add_typed_entry("done", 1, result("c1", false))
        .unwrap();
    let messages = conversation_messages(&region);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].role, Role::User);
}

#[test]
fn media_an_assistant_made_follows_it_in_a_user_turn() {
    let mut region = history();
    let drawn = EntryContent::from_parts(vec![Part::text("here"), stored("cat.png")]);
    region
        .add_typed_entry(
            drawn.clone(),
            1,
            EntryKind::AssistantTurn { tool_calls: vec![] },
        )
        .unwrap();
    let messages = conversation_messages(&region);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, Role::Assistant);
    assert_eq!(
        messages[0].content,
        ConversationContent::Text(drawn.to_string())
    );
    assert_eq!(messages[1].role, Role::User);
    assert!(matches!(
        &messages[1].content,
        ConversationContent::Blocks(b) if matches!(b[..], [ConversationBlock::Stored(_)])
    ));
}

/// A turn that called tools keeps its media as a stand-in: a user turn in
/// between would split the call from its result.
#[test]
fn media_on_a_turn_with_calls_stays_a_stand_in() {
    let mut region = history();
    region
        .add_typed_entry(
            EntryContent::from_parts(vec![Part::text("look"), stored("dog.png")]),
            1,
            EntryKind::AssistantTurn {
                tool_calls: vec![call("c1")],
            },
        )
        .unwrap();
    let messages = conversation_messages(&region);
    assert_eq!(messages.len(), 1);
    let ConversationContent::Blocks(blocks) = &messages[0].content else {
        panic!("a turn with calls is blocks");
    };
    assert_eq!(blocks[0], ConversationBlock::Text("look".into()));
    assert_eq!(
        blocks[1],
        ConversationBlock::Text("[image/png, 3 B] dog.png".into())
    );
    assert!(matches!(blocks[2], ConversationBlock::ToolUse(_)));
}

#[test]
fn prefixed_text_entries_read_as_the_role_they_name() {
    let mut region = history();
    region.add_entry("  Assistant: hi", 1).unwrap();
    region.add_entry("User: hello", 1).unwrap();
    region.add_entry("plain note", 1).unwrap();
    region
        .add_entry(
            EntryContent::from_parts(vec![Part::text("see"), stored("a.png")]),
            1,
        )
        .unwrap();
    let messages = conversation_messages(&region);
    assert_eq!(
        messages[0],
        ConversationMessage {
            role: Role::Assistant,
            content: ConversationContent::Text("hi".into()),
            reasoning: None,
        }
    );
    assert_eq!(
        messages[1].content,
        ConversationContent::Text("hello".into())
    );
    assert_eq!(messages[1].role, Role::User);
    assert_eq!(
        messages[2].content,
        ConversationContent::Text("plain note".into())
    );
    assert!(matches!(
        &messages[3].content,
        ConversationContent::Blocks(b) if b.len() == 3
    ));
}

#[test]
fn assemble_context_renders_the_conversation_and_hands_back_custom_regions() {
    let mut task = Region::new("task".into(), RegionKind::Pinned, 1_000);
    task.add_entry("do it", 2).unwrap();
    let mut talk = history();
    talk.add_typed_entry("go", 1, EntryKind::UserMessage)
        .unwrap();
    let custom = Region::new(
        "notes".into(),
        RegionKind::Custom {
            script: "notes.rhai".into(),
            pinned: false,
        },
        1_000,
    );
    let mut handed_back = Vec::new();
    let assembled = super::super::assemble_context([&task, &talk, &custom], |custom, _| {
        handed_back.push((custom.region.name.clone(), custom.script, custom.pinned));
    });
    assert_eq!(handed_back, [("notes".to_string(), "notes.rhai", false)]);
    assert_eq!(assembled.system.blocks.len(), 1);
    assert_eq!(assembled.conversation.len(), 1);
    assert_eq!(assembled.conversation[0].role, Role::User);
}
