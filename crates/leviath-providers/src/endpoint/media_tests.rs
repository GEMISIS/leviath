//! What a model behind a custom endpoint takes: the vendor prefix, then the
//! operator's override.

use super::*;
use leviath_core::media::MediaType;

#[test]
fn a_gateway_answers_by_vendor_prefix_and_the_override_wins() {
    let provider = EndpointProvider::new(
        reqwest::Client::new(),
        "gw",
        "http://localhost:1/v1",
        None,
        Vec::new(),
    );
    assert!(
        provider
            .media("openai/gpt-5.5")
            .accepts(&MediaType::parse("image/png").unwrap())
    );
    assert!(!provider.media("qwen3-8b").takes_media());
    let provider = provider.with_overrides(HashMap::from([(
        "qwen3-8b".to_string(),
        ModelCapabilityOverride {
            input_types: Some(vec!["text/*".into(), "image/*".into()]),
            ..Default::default()
        },
    )]));
    assert!(provider.media("qwen3-8b").takes_media());
}
