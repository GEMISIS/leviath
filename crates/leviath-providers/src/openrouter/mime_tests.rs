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
