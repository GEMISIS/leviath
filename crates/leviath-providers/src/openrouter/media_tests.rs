//! What a gateway model takes: the vendor prefix, the listing, the override.

use super::*;
use crate::capabilities::ModelCapabilityOverride;
use crate::learned::LearnedModel;
use leviath_core::media::MediaType;

#[test]
fn the_prefix_guesses_and_the_listing_corrects() {
    let mut provider = OpenRouterProvider::new(reqwest::Client::new(), "k".to_string());
    assert!(
        provider
            .media("anthropic/claude-sonnet-5")
            .accepts(&MediaType::parse("image/png").unwrap())
    );
    assert!(!provider.media("deepseek/deepseek-v4").takes_media());
    provider.learned.replace(std::collections::HashMap::from([(
        "deepseek/deepseek-v4".to_string(),
        LearnedModel {
            input_types: Some(vec!["text/*".into(), "image/*".into()]),
            ..Default::default()
        },
    )]));
    assert!(provider.media("deepseek/deepseek-v4").takes_media());
    provider.capability_overrides.insert(
        "deepseek/deepseek-v4".to_string(),
        ModelCapabilityOverride {
            input_types: Some(vec!["text/*".into()]),
            ..Default::default()
        },
    );
    assert!(!provider.media("deepseek/deepseek-v4").takes_media());
}
