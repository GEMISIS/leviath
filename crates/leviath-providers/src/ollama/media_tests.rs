//! What a local model takes: its name, then `/api/show`, then the override.

use super::*;
use crate::capabilities::ModelCapabilityOverride;
use crate::learned::LearnedModel;
use leviath_core::media::MediaType;

#[test]
fn vision_builds_by_name_then_by_show_then_by_override() {
    let mut provider = OllamaProvider::new(reqwest::Client::new());
    assert!(
        provider
            .media("llava:13b")
            .accepts(&MediaType::parse("image/png").unwrap())
    );
    assert!(!provider.media("llama3.3").takes_media());
    provider.learned.replace(std::collections::HashMap::from([(
        "llama3.3".to_string(),
        LearnedModel {
            input_types: Some(vec!["text/*".into(), "image/*".into()]),
            ..Default::default()
        },
    )]));
    assert!(provider.media("llama3.3").takes_media());
    provider.capability_overrides.insert(
        "llama3.3".to_string(),
        ModelCapabilityOverride {
            input_types: Some(vec!["text/*".into()]),
            ..Default::default()
        },
    );
    assert!(!provider.media("llama3.3").takes_media());
}

#[test]
fn a_media_block_is_charged_at_its_registry_estimate() {
    use leviath_core::media::{Blob, MediaRegistry, Part};
    let reg = MediaRegistry::builtin();
    let blob = Blob::new(MediaType::parse("image/png").unwrap(), vec![1, 2, 3]).named("a.png");
    let part = Part::stored(blob.describe(&reg)).named("a.png");
    let block = crate::ContentBlock::media(&part).unwrap();
    assert_eq!(estimated_block_tokens(&block), 1600);
}
