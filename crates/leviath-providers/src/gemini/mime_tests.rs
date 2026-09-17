//! What Gemini models take: the family table, the listing, the override.

use super::*;
use crate::capabilities::ModelCapabilityOverride;
use crate::learned::LearnedModel;
use leviath_core::mime::MimeType;

#[test]
fn the_family_table_then_the_listing_then_the_override() {
    let mut provider = GeminiProvider::new(reqwest::Client::new(), "k".to_string());
    // The table says the model takes audio and video, and the Interactions
    // shape the requests go out in has a slot for both.
    let table = provider.mime("gemini-3.5-flash");
    assert!(table.accepts(&MimeType::parse("audio/wav").unwrap()));
    assert!(table.accepts(&MimeType::parse("video/mp4").unwrap()));
    provider.learned.replace(std::collections::HashMap::from([(
        "gemini-3.5-flash".to_string(),
        LearnedModel {
            input_types: Some(vec!["text/*".into()]),
            ..Default::default()
        },
    )]));
    assert!(!provider.mime("gemini-3.5-flash").takes_mime());
    provider.capability_overrides.insert(
        "gemini-3.5-flash".to_string(),
        ModelCapabilityOverride {
            input_types: Some(vec!["text/*".into(), "audio/*".into()]),
            ..Default::default()
        },
    );
    let mime = provider.mime("gemini-3.5-flash");
    assert!(mime.accepts(&MimeType::parse("audio/wav").unwrap()));
    assert!(!mime.accepts(&MimeType::parse("video/mp4").unwrap()));
}
