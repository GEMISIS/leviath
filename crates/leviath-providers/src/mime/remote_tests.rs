//! Parts the vendor holds, and parts over a vendor's inline limit.

use super::*;
use crate::files::{MIB, MediaLimits, RemoteFile};
use crate::provider::Message;
use leviath_core::mime::{Blob, Part};

fn part(mime: &str, bytes: &[u8], name: &str) -> Part {
    let blob = Blob::new(MimeType::parse(mime).unwrap(), bytes.to_vec()).named(name);
    Part::stored(blob.describe(&MimeRegistry::builtin())).named(name)
}

fn remote(block: ContentBlock, id: &str) -> ContentBlock {
    match block {
        ContentBlock::Mime {
            part,
            data,
            name,
            deliver,
            ..
        } => ContentBlock::Mime {
            part,
            data,
            name,
            deliver,
            remote: Some(RemoteFile {
                id: id.into(),
                uri: None,
                expires_at: None,
            }),
        },
        other => other,
    }
}

fn request(blocks: Vec<ContentBlock>) -> InferenceRequest {
    InferenceRequest {
        system: Vec::new(),
        messages: vec![Message {
            role: "user".into(),
            content: MessageContent::Blocks(blocks),
            cache_breakpoint: false,
            reasoning: None,
        }],
        model: "m".into(),
        max_tokens: 10,
        temperature: 0.0,
        tools: Vec::new(),
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    }
}

fn blocks(request: &InferenceRequest) -> &[ContentBlock] {
    match &request.messages[0].content {
        MessageContent::Blocks(blocks) => blocks,
        MessageContent::Text(_) => &[],
    }
}

#[test]
fn a_part_the_vendor_holds_is_named_by_id_in_every_shape() {
    let pdf = remote(
        ContentBlock::mime(&part("application/pdf", b"%PDF", "a.pdf")).unwrap(),
        "file_1",
    );
    let png = remote(
        ContentBlock::mime(&part("image/png", b"\x89PNG\r\n\x1a\n", "b.png")).unwrap(),
        "file_2",
    );
    let wav = remote(
        ContentBlock::mime(&part("audio/wav", b"RIFF", "c.wav")).unwrap(),
        "file_3",
    );
    let mp4 = remote(
        ContentBlock::mime(&part("video/mp4", b"....ftyp", "d.mp4")).unwrap(),
        "file_4",
    );
    assert!(pdf.is_hydrated_mime(), "a named copy counts as sent");

    assert_eq!(
        anthropic_block(&pdf).unwrap(),
        serde_json::json!({ "type": "document", "source": { "type": "file", "file_id": "file_1" } })
    );
    assert_eq!(
        anthropic_block(&png).unwrap(),
        serde_json::json!({ "type": "image", "source": { "type": "file", "file_id": "file_2" } })
    );
    assert_eq!(anthropic_block(&wav).unwrap()["type"], "text");

    assert_eq!(
        responses_part(&pdf).unwrap(),
        serde_json::json!({ "type": "input_file", "file_id": "file_1" })
    );
    assert_eq!(responses_part(&png).unwrap()["type"], "input_image");
    assert_eq!(responses_part(&wav).unwrap()["type"], "input_audio");
    assert_eq!(responses_part(&mp4).unwrap()["type"], "input_video");

    assert_eq!(
        openai_part(&pdf).unwrap(),
        serde_json::json!({ "type": "file", "file": { "file_id": "file_1" } })
    );
    assert_eq!(
        openai_part(&png).unwrap()["type"],
        "text",
        "Chat Completions names no image by id"
    );

    let req = request(vec![pdf.clone(), png.clone()]);
    assert_eq!(mime_tokens(&req), block_tokens(&pdf) + block_tokens(&png));
}

#[test]
fn hydration_leaves_a_held_part_alone_and_holds_back_one_over_the_inline_limit() {
    let held = remote(
        ContentBlock::mime(&part("application/pdf", b"%PDF", "held.pdf")).unwrap(),
        "file_1",
    );
    let big = ContentBlock::mime(&part("image/png", &[0u8; 64], "big.png")).unwrap();
    let small = ContentBlock::mime(&part("application/pdf", b"%PDF-small", "s.pdf")).unwrap();
    let mut req = request(vec![held, big, small]);
    let fetched = std::cell::Cell::new(0);
    let fetch = |_: &BlobRef| {
        fetched.set(fetched.get() + 1);
        Some(Arc::from(&b"%PDF-small"[..]))
    };
    let limits = MediaLimits {
        inline_part_bytes: &[("image/*", 16)],
        ..MediaLimits::NONE
    };
    let mime = ModelMime::new(&["text/*", "image/*", "application/pdf"], &["text/*"]);
    let report = hydrate_request(
        &mut req,
        &Hydration {
            mime: &mime,
            registry: &MimeRegistry::builtin(),
            max_media_bytes: 12,
            fetch: &fetch,
            limits,
            why_inline: "zero data retention is on, so nothing is uploaded",
        },
    );
    assert_eq!(report.by_file, 1);
    assert_eq!(report.too_large, 1);
    assert_eq!(
        report.sent, 1,
        "neither held nor too-large bytes count on the cap"
    );
    assert_eq!(fetched.get(), 1, "only the inline part is read");
    let out = blocks(&req);
    assert!(
        matches!(&out[0], ContentBlock::Mime { remote: Some(f), data, .. } if f.id == "file_1" && data.is_empty())
    );
    let ContentBlock::Text { text } = &out[1] else {
        panic!("a text stand-in: {:?}", out[1]);
    };
    assert!(text.contains("big.png"), "{text}");
    assert!(
        text.contains("over the 0.0 MiB this provider takes inline; zero data retention is on"),
        "{text}"
    );
    assert!(out[2].is_hydrated_mime());

    let mut plain = request(vec![
        ContentBlock::mime(&part("image/png", &[0u8; 64], "big.png")).unwrap(),
    ]);
    hydrate_request(
        &mut plain,
        &Hydration {
            mime: &mime,
            registry: &MimeRegistry::builtin(),
            max_media_bytes: MIB,
            fetch: &fetch,
            limits,
            why_inline: "",
        },
    );
    let ContentBlock::Text { text } = &blocks(&plain)[0] else {
        panic!("a text stand-in");
    };
    assert!(text.ends_with("takes inline]"), "{text}");
}

#[test]
fn a_part_sent_natively_is_one_the_model_takes_and_no_override_turns_aside() {
    let mime = ModelMime::new(&["text/*", "image/*"], &["text/*"]);
    let png = ContentBlock::mime(&part("image/png", b"\x89PNG\r\n\x1a\n", "b.png")).unwrap();
    assert!(sends_natively(&png, &mime));
    let as_text = match png.clone() {
        ContentBlock::Mime {
            part,
            data,
            name,
            remote,
            ..
        } => ContentBlock::Mime {
            part,
            data,
            name,
            remote,
            deliver: Some(leviath_core::mime::Delivery::StandIn),
        },
        other => other,
    };
    assert!(!sends_natively(&as_text, &mime));
    let pdf = ContentBlock::mime(&part("application/pdf", b"%PDF", "a.pdf")).unwrap();
    assert!(!sends_natively(&pdf, &mime));
    assert!(!sends_natively(
        &ContentBlock::Text { text: "t".into() },
        &mime
    ));
}
