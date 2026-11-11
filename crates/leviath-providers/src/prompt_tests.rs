use super::*;
use crate::capabilities::ModelMime;
use crate::provider::{OPENING_TURN, PROMPT_REGIONS_KEY, SystemBlock};
use leviath_core::mime::{MimeRegistry, MimeType, Part};

fn region(name: &str, text: &str) -> SystemBlock {
    SystemBlock {
        text: format!("## {name}\n{text}"),
        cache_hint: leviath_core::CacheHint::Always,
        region: name.into(),
        volatility: leviath_core::Volatility::Stable,
    }
}

fn user(content: MessageContent) -> Message {
    Message {
        role: "user".into(),
        content,
        cache_breakpoint: false,
        reasoning: None,
    }
}

/// What the runtime assembles for a draw stage whose prompt lives in pinned
/// regions: the regions in the system blocks, a hint block, and the bare
/// opening turn as the only message.
fn draw_stage() -> InferenceRequest {
    InferenceRequest {
        system: vec![
            SystemBlock {
                text: "Work in the project's folder.".into(),
                cache_hint: leviath_core::CacheHint::Always,
                region: String::new(),
                volatility: leviath_core::Volatility::Stable,
            },
            region("items", "boilerplate about the whole project"),
            region("brief", "2D pixel art, 32px tiles, mossy greens"),
            region("piece_1", "temple_brick: a weathered stone brick"),
        ],
        messages: vec![user(MessageContent::Text(OPENING_TURN.into()))],
        model: "black-forest-labs/flux.2-flex".into(),
        max_tokens: 100,
        temperature: 0.0,
        tools: vec![],
        extra: serde_json::json!({ PROMPT_REGIONS_KEY: ["brief", "piece_1"], "seed": 7 }),
        request_timeout_secs: None,
    }
}

fn image() -> ContentBlock {
    let bytes = b"PNGBYTES".to_vec();
    let blob =
        leviath_core::mime::Blob::new(MimeType::parse("image/png").unwrap(), bytes).named("a.png");
    let part = Part::stored(blob.describe(&MimeRegistry::builtin())).named("a.png");
    ContentBlock::Mime {
        part: part.blob().unwrap().clone(),
        data: "UE5HQllURVM=".into(),
        name: part.name.clone(),
        deliver: None,
        remote: None,
    }
}

/// Live run 8: an image model behind a chat route was handed the system
/// blocks and "Begin.", and drew the word. Read as one prompt, the request is
/// exactly the regions the stage named, as the only user turn, with nothing
/// in the system and the regions' key spent.
#[test]
fn a_draw_stage_becomes_its_named_prompt_and_nothing_else() {
    let request = prompt_only(draw_stage());
    assert!(request.system.is_empty(), "{:?}", request.system);
    assert_eq!(request.messages.len(), 1);
    assert_eq!(request.messages[0].role, "user");
    assert_eq!(
        request.messages[0].content,
        MessageContent::Text(
            "2D pixel art, 32px tiles, mossy greens\ntemple_brick: a weathered stone brick".into()
        )
    );
    assert_eq!(request.extra, serde_json::json!({ "seed": 7 }));
    // Read again by a media route, it is the same prompt.
    assert_eq!(
        request_text(&request),
        "2D pixel art, 32px tiles, mossy greens\ntemple_brick: a weathered stone brick"
    );
}

/// A part the stage handed the model (a picture to start from) stays, ahead
/// of the prompt; the pointer text beside it does not become the prompt.
#[test]
fn the_parts_a_request_carries_go_with_the_prompt() {
    let png = image();
    let pointer = format!("[drawn] {}", png.stand_in().unwrap());
    let mut request = draw_stage();
    request.extra = serde_json::Value::Null;
    request.messages = vec![
        user(MessageContent::Text(OPENING_TURN.into())),
        user(MessageContent::Blocks(vec![
            ContentBlock::Text { text: pointer },
            png.clone(),
        ])),
    ];
    let request = prompt_only(request);
    assert_eq!(request.messages.len(), 1);
    assert_eq!(
        request.messages[0].content,
        MessageContent::Blocks(vec![
            png,
            ContentBlock::Text {
                text: "boilerplate about the whole project\n2D pixel art, 32px tiles, mossy \
                       greens\ntemple_brick: a weathered stone brick"
                    .into()
            },
        ])
    );
    assert!(!unprompted(&request));
}

/// With nothing to make a prompt of, the request is handed back as it was,
/// and that is what `unprompted` says to refuse. A part alone is something to
/// work from (an image to upscale, a mesh to rig); so is a prompt.
#[test]
fn a_request_that_is_only_the_opening_turn_is_unprompted() {
    let mut bare = draw_stage();
    bare.extra = serde_json::json!({ PROMPT_REGIONS_KEY: ["never_written"] });
    assert_eq!(
        serde_json::to_value(prompt_only(bare.clone())).unwrap(),
        serde_json::to_value(&bare).unwrap()
    );
    assert!(unprompted(&bare));

    let mut nothing = draw_stage();
    nothing.system.clear();
    nothing.extra = serde_json::Value::Null;
    assert!(unprompted(&nothing));

    let mut part_only = nothing.clone();
    part_only
        .messages
        .push(user(MessageContent::Blocks(vec![image()])));
    assert!(!unprompted(&part_only));

    assert!(!unprompted(&draw_stage()));
}

/// Whether a model answers in text is read off what it makes: an image,
/// audio or video model does not; a chat model, a model that also draws, and
/// a model nothing has described do.
#[test]
fn a_model_answers_in_text_when_any_of_its_outputs_is_text() {
    assert!(!ModelMime::new(&["text/*"], &["image/*"]).answers_in_text());
    assert!(!ModelMime::new(&["text/*"], &["audio/mpeg", "video/mp4"]).answers_in_text());
    assert!(ModelMime::new(&["text/*"], &["text/*", "image/*"]).answers_in_text());
    assert!(ModelMime::new(&["text/*"], &["text/markdown"]).answers_in_text());
    assert!(ModelMime::new(&["*/*"], &["*/*"]).answers_in_text());
    assert!(ModelMime::new(&["text/*"], &[]).answers_in_text());
    assert!(ModelMime::text_only().answers_in_text());
}
