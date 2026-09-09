//! What Claude models take: the table, then the operator's override.

use super::*;
use crate::capabilities::ModelCapabilityOverride;
use leviath_core::media::MediaType;

#[test]
fn every_claude_reads_images_and_pdfs_until_an_override_says_otherwise() {
    let mut provider = AnthropicProvider::new(reqwest::Client::new(), "k".to_string());
    let media = provider.media("claude-sonnet-5");
    assert!(media.accepts(&MediaType::parse("image/png").unwrap()));
    assert!(media.accepts(&MediaType::parse("application/pdf").unwrap()));
    assert!(!media.accepts(&MediaType::parse("audio/wav").unwrap()));
    provider.capability_overrides.insert(
        "claude-sonnet-5".to_string(),
        ModelCapabilityOverride {
            input_types: Some(vec!["text/*".into()]),
            ..Default::default()
        },
    );
    assert!(!provider.media("claude-sonnet-5").takes_media());
    assert!(provider.media("claude-opus-5").takes_media());
}
