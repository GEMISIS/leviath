//! That a configured gateway URL is the one actually called.
//!
//! Every keyed provider already held a `base_url` and every one honoured it in
//! `with_config` - but the registry builds through `with_overrides`, which set
//! the vendor default and ignored configuration entirely. So the setting
//! existed, was tested, and could not be reached from a config file.
//!
//! These prove the whole path rather than the field: each provider is pointed
//! at a local mock and has to come back carrying that server's answer. If the
//! URL were dropped the call would leave for the vendor's real host instead,
//! and the distinctive status below would not come back.

use crate::Provider;
use crate::provider::{InferenceRequest, ProviderError};

/// A status no vendor returns for a bad key, so seeing it back is proof of
/// where the request went rather than of what the key was.
const TEAPOT: u16 = 418;

fn request() -> InferenceRequest {
    InferenceRequest {
        system: Vec::new(),
        messages: vec![crate::provider::Message {
            role: "user".to_string(),
            content: "hi".to_string().into(),
            cache_breakpoint: false,
        }],
        model: "some-internal-model".to_string(),
        max_tokens: 16,
        temperature: 0.0,
        tools: Vec::new(),
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    }
}

fn client() -> reqwest::Client {
    crate::provider::build_http_client(None).expect("a test client builds")
}

/// The error text a provider hands back after calling `url`.
async fn reached(provider: &dyn Provider) -> String {
    match provider.infer(&request()).await {
        Ok(_) => String::new(),
        Err(e) => e.to_string(),
    }
}

async fn mock() -> String {
    leviath_testkit::spawn_mock_server(TEAPOT, "I am a teapot", b"{}").await
}

#[tokio::test]
async fn anthropic_calls_the_gateway_it_was_given() {
    let url = mock().await;
    let provider = crate::AnthropicProvider::with_overrides(
        client(),
        "k".to_string(),
        Default::default(),
        None,
    )
    .with_base_url(Some(url));

    let err = reached(&provider).await;

    assert!(
        err.contains("418"),
        "the gateway answered, not the vendor: {err}"
    );
}

#[tokio::test]
async fn openai_calls_the_gateway_it_was_given() {
    let url = mock().await;
    let provider =
        crate::OpenAIProvider::with_overrides(client(), "k".to_string(), Default::default(), None)
            .with_base_url(Some(url));

    let err = reached(&provider).await;

    assert!(
        err.contains("418"),
        "the gateway answered, not the vendor: {err}"
    );
}

#[tokio::test]
async fn google_calls_the_gateway_it_was_given() {
    let url = mock().await;
    let provider =
        crate::GeminiProvider::with_overrides(client(), "k".to_string(), Default::default(), None)
            .with_base_url(Some(url));

    let err = reached(&provider).await;

    assert!(
        err.contains("418"),
        "the gateway answered, not the vendor: {err}"
    );
}

#[tokio::test]
async fn openrouter_calls_the_gateway_it_was_given() {
    let url = mock().await;
    let provider = crate::OpenRouterProvider::with_overrides(
        client(),
        "k".to_string(),
        Default::default(),
        None,
    )
    .with_base_url(Some(url));

    let err = reached(&provider).await;

    assert!(
        err.contains("418"),
        "the gateway answered, not the vendor: {err}"
    );
}

/// Saying nothing leaves the host alone rather than clearing it. That is the
/// branch a config without a gateway takes, so it has to mean "unchanged" and
/// not "reset" - on every provider, since each carries its own copy.
#[tokio::test]
async fn no_gateway_leaves_the_host_as_it_was() {
    let anthropic = crate::AnthropicProvider::with_overrides(
        client(),
        "k".to_string(),
        Default::default(),
        None,
    )
    .with_base_url(Some(mock().await))
    .with_base_url(None);
    let openai =
        crate::OpenAIProvider::with_overrides(client(), "k".to_string(), Default::default(), None)
            .with_base_url(Some(mock().await))
            .with_base_url(None);
    let google =
        crate::GeminiProvider::with_overrides(client(), "k".to_string(), Default::default(), None)
            .with_base_url(Some(mock().await))
            .with_base_url(None);
    let openrouter = crate::OpenRouterProvider::with_overrides(
        client(),
        "k".to_string(),
        Default::default(),
        None,
    )
    .with_base_url(Some(mock().await))
    .with_base_url(None);

    for provider in [&anthropic as &dyn Provider, &openai, &google, &openrouter] {
        let err = reached(provider).await;
        assert!(
            err.contains("418"),
            "None must not undo the host already set: {err}"
        );
    }
}

/// A gateway error must not be mistaken for the provider being out of service:
/// the circuit breaker takes a provider out for everyone on an auth or credit
/// failure, and a misconfigured URL is neither.
#[tokio::test]
async fn a_gateway_that_refuses_is_not_read_as_an_unavailable_provider() {
    let url = mock().await;
    let provider = crate::AnthropicProvider::with_overrides(
        client(),
        "k".to_string(),
        Default::default(),
        None,
    )
    .with_base_url(Some(url));

    let err = provider
        .infer(&request())
        .await
        .expect_err("the mock refuses");

    assert!(
        !matches!(err, ProviderError::Unavailable { .. }),
        "a 418 from a gateway is not a reason to take the provider out of service: {err}"
    );
}
