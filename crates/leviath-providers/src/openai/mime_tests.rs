//! What OpenAI models take: the table, then the operator's override.

use super::*;
use crate::capabilities::ModelCapabilityOverride;
use leviath_core::mime::MimeType;

#[test]
fn the_table_answers_and_an_override_corrects_it() {
    let mut provider = OpenAIProvider::new(reqwest::Client::new(), "k".to_string());
    assert!(
        provider
            .mime("gpt-5.5")
            .accepts(&MimeType::parse("image/png").unwrap())
    );
    assert!(!provider.mime("gpt-3.5-turbo").takes_mime());
    provider.capability_overrides.insert(
        "gpt-3.5-turbo".to_string(),
        ModelCapabilityOverride {
            input_types: Some(vec!["text/*".into(), "image/*".into()]),
            ..Default::default()
        },
    );
    assert!(provider.mime("gpt-3.5-turbo").takes_mime());
}
