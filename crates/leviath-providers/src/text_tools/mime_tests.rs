//! Stored parts on a text-only transport.

use super::*;
use leviath_core::mime::{Blob, MimeRegistry, MimeType, Part};

#[test]
fn a_flattened_transcript_names_the_part_by_its_stand_in() {
    let reg = MimeRegistry::builtin();
    let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![1, 2, 3]).named("a.png");
    let part = Part::stored(blob.describe(&reg)).named("a.png");
    let messages = vec![Message {
        role: "user".to_string(),
        content: MessageContent::Blocks(vec![
            ContentBlock::Text { text: "see".into() },
            ContentBlock::mime(&part).unwrap(),
        ]),
        cache_breakpoint: false,
        reasoning: None,
    }];
    let text = flatten_messages(&messages);
    assert!(text.contains("see\n[image/png, 3 B] a.png"), "{text}");
    let first = vec![Message {
        role: "user".to_string(),
        content: MessageContent::Blocks(vec![
            ContentBlock::mime(&part).unwrap(),
            ContentBlock::Text {
                text: "after".into(),
            },
        ]),
        cache_breakpoint: false,
        reasoning: None,
    }];
    let text = flatten_messages(&first);
    assert!(text.contains("[image/png, 3 B] a.png\nafter"), "{text}");
}
