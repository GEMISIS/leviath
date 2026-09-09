//! What Gemini models take: the family table, the listing, the override.

use super::*;
use crate::capabilities::ModelCapabilityOverride;
use crate::learned::LearnedModel;
use leviath_core::media::MediaType;

#[test]
fn the_family_table_then_the_listing_then_the_override() {
    let mut provider = GeminiProvider::new(reqwest::Client::new(), "k".to_string());
    assert!(
        provider
            .media("gemini-3.5-flash")
            .accepts(&MediaType::parse("video/mp4").unwrap())
    );
    provider.learned.replace(std::collections::HashMap::from([(
        "gemini-3.5-flash".to_string(),
        LearnedModel {
            input_types: Some(vec!["text/*".into()]),
            ..Default::default()
        },
    )]));
    assert!(!provider.media("gemini-3.5-flash").takes_media());
    provider.capability_overrides.insert(
        "gemini-3.5-flash".to_string(),
        ModelCapabilityOverride {
            input_types: Some(vec!["text/*".into(), "audio/*".into()]),
            ..Default::default()
        },
    );
    let media = provider.media("gemini-3.5-flash");
    assert!(media.accepts(&MediaType::parse("audio/wav").unwrap()));
    assert!(!media.accepts(&MediaType::parse("video/mp4").unwrap()));
}
