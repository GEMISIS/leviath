//! What a local model takes: its name, then `/api/show`, then the override.

use super::*;
use crate::capabilities::ModelCapabilityOverride;
use crate::learned::LearnedModel;
use leviath_core::mime::MimeType;

#[test]
fn vision_builds_by_name_then_by_show_then_by_override() {
    let mut provider = OllamaProvider::new(reqwest::Client::new());
    assert!(
        provider
            .mime("llava:13b")
            .accepts(&MimeType::parse("image/png").unwrap())
    );
    assert!(!provider.mime("llama3.3").takes_mime());
    provider.learned.replace(std::collections::HashMap::from([(
        "llama3.3".to_string(),
        LearnedModel {
            input_types: Some(vec!["text/*".into(), "image/*".into()]),
            ..Default::default()
        },
    )]));
    assert!(provider.mime("llama3.3").takes_mime());
    provider.capability_overrides.insert(
        "llama3.3".to_string(),
        ModelCapabilityOverride {
            input_types: Some(vec!["text/*".into()]),
            ..Default::default()
        },
    );
    assert!(!provider.mime("llama3.3").takes_mime());
}

#[test]
fn a_mime_block_is_charged_at_its_registry_estimate() {
    use leviath_core::mime::{Blob, MimeRegistry, Part};
    let reg = MimeRegistry::builtin();
    let blob = Blob::new(MimeType::parse("image/png").unwrap(), vec![1, 2, 3]).named("a.png");
    let part = Part::stored(blob.describe(&reg)).named("a.png");
    let block = crate::ContentBlock::mime(&part).unwrap();
    assert_eq!(estimated_block_tokens(&block), 1600);
}
