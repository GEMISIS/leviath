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

/// What the runtime assembles for a stage that names two prompt regions, one
/// of them holding a caption and a stored picture: the picture's stand-in is
/// a line of that region's text, and its bytes are lifted into a user message
/// behind a pointer.
fn stage_with_a_picture_in_its_prompt() -> (InferenceRequest, ContentBlock) {
    let png = image();
    let stand_in = png.stand_in().unwrap().to_string();
    let mut request = draw_stage();
    request.system = vec![
        region("render_brief", "a brief"),
        region("frame", &format!("caption\n{stand_in}")),
    ];
    request.messages = vec![
        user(MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: format!("[frame] {stand_in}"),
            },
            png.clone(),
        ])),
        user(MessageContent::Text(OPENING_TURN.into())),
    ];
    request.extra = serde_json::json!({ PROMPT_REGIONS_KEY: ["render_brief", "frame"] });
    (request, png)
}

/// A storyboard's render stage and a film's shoot stage each named a region
/// holding a reference picture as part of the prompt. The prompt then held
/// the picture's stand-in, and a media route reading the one-prompt request
/// took the whole prompt for the picture's pointer and found no text at all:
/// "needs a prompt: the stage handed it no text", whenever a picture was
/// attached. The prompt is the regions' words, read once or twice, and the
/// picture travels as the part it is.
#[test]
fn a_prompt_region_holding_a_picture_keeps_its_words_and_loses_the_stand_in() {
    let (request, png) = stage_with_a_picture_in_its_prompt();
    let request = prompt_only(request);
    assert_eq!(
        request.messages[0].content,
        MessageContent::Blocks(vec![
            png,
            ContentBlock::Text {
                text: "a brief\ncaption".into()
            },
        ])
    );
    assert_eq!(request_text(&request), "a brief\ncaption");
    assert!(!unprompted(&request));
}

/// A pointer is the stand-in alone or behind the region (and key) it came
/// from, on one line. Text that mentions a part among its own words is a
/// prompt, and keeps every word but a line that is the pointer.
#[test]
fn only_a_line_that_is_a_pointer_is_left_out_of_the_prompt() {
    let png = image();
    let stand_in = png.stand_in().unwrap().to_string();
    let mut request = draw_stage();
    request.system.clear();
    request.extra = serde_json::Value::Null;
    let texts = [
        stand_in.clone(),
        format!("[drawn] {stand_in}"),
        format!("[drawn / hero] {stand_in}"),
        format!("match the palette of {stand_in}"),
        format!("a porch at dusk\n  [drawn] {stand_in}  \nseen from the lane"),
    ];
    let mut blocks: Vec<ContentBlock> = texts
        .into_iter()
        .map(|text| ContentBlock::Text { text })
        .collect();
    blocks.push(png);
    request.messages = vec![user(MessageContent::Blocks(blocks))];
    assert_eq!(
        request_text(&request),
        format!("match the palette of {stand_in}\na porch at dusk\nseen from the lane")
    );

    // A region whose text is a caption and a stand-in is read for its caption
    // when the conversation holds no words, and one that is only the stand-in
    // still adds nothing.
    let (mut pinned, _) = stage_with_a_picture_in_its_prompt();
    pinned.extra = serde_json::Value::Null;
    assert_eq!(request_text(&pinned), "a brief\ncaption");
    pinned.system[1] = region("frame", &stand_in);
    assert_eq!(request_text(&pinned), "a brief");
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
