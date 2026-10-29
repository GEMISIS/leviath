//! What a gateway model takes: the vendor prefix, the listing, the override.

use super::*;
use crate::capabilities::ModelCapabilityOverride;
use crate::learned::LearnedModel;
use leviath_core::mime::MimeType;

#[test]
fn the_prefix_guesses_and_the_listing_corrects() {
    let mut provider = OpenRouterProvider::new(reqwest::Client::new(), "k".to_string());
    assert!(
        provider
            .mime("anthropic/claude-sonnet-5")
            .accepts(&MimeType::parse("image/png").unwrap())
    );
    assert!(!provider.mime("deepseek/deepseek-v4").takes_mime());
    provider.learned.replace(std::collections::HashMap::from([(
        "deepseek/deepseek-v4".to_string(),
        LearnedModel {
            input_types: Some(vec!["text/*".into(), "image/*".into()]),
            ..Default::default()
        },
    )]));
    assert!(provider.mime("deepseek/deepseek-v4").takes_mime());
    provider.capability_overrides.insert(
        "deepseek/deepseek-v4".to_string(),
        ModelCapabilityOverride {
            input_types: Some(vec!["text/*".into()]),
            ..Default::default()
        },
    );
    assert!(!provider.mime("deepseek/deepseek-v4").takes_mime());
}

/// Asked for wav, a model answered an MP3: the part is typed by its bytes.
#[test]
fn streamed_audio_asked_for_as_wav_is_typed_by_its_mp3_bytes() {
    let mut audio = AudioStreamAccumulator::new(Some("audio/wav"), "wav");
    let mut event = "data: {\"choices\":[{\"delta\":{\"audio\":{\"data\":\"//uQZAA=\",\"format\":\"wav\"}},\"finish_reason\":\"stop\"}]}\n\n".to_string();
    let chunk = openrouter_audio_sse_event(&mut event, &mut audio)
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(chunk.parts.len(), 1);
    assert_eq!(chunk.parts[0].mime_type.as_str(), "audio/mpeg");
    assert_eq!(chunk.parts[0].bytes, b"\xFF\xFB\x90\x64\x00");
}
