//! Manual check of OpenRouter's Images API route against the live gateway.
//!
//! Ignored and env-gated: it draws one small picture on a real OpenRouter key
//! (a few cents at most). It reads the public model listing first, so the
//! model is routed the way a stage's would be: by the `endpoints` the listing
//! names for it, not by its name. Run it with a key of your own:
//!
//! ```text
//! LEVIATH_LIVE_OPENROUTER_IMAGES=1 OPENROUTER_API_KEY=<key> \
//!     cargo test -p leviath-providers --test openrouter_images_live -- --ignored --nocapture
//! ```
//!
//! `LEVIATH_LIVE_OPENROUTER_IMAGE_MODEL` names another model to draw with
//! (default `black-forest-labs/flux-3-image-klein`, one chat refuses). The
//! stage asks for `image/png`: the default model's images listing names
//! `png`, so the part must be PNG; another model must answer PNG only when
//! its listing names `png`.

#![cfg(feature = "openrouter")]

use leviath_providers::Provider as _;
use leviath_providers::openrouter::OpenRouterProvider;
use leviath_providers::provider::{InferenceRequest, Message, MessageContent};

#[tokio::test]
#[ignore = "spends a little of a real OpenRouter key; needs LEVIATH_LIVE_OPENROUTER_IMAGES=1"]
async fn one_picture_comes_back_from_the_images_api() {
    if std::env::var("LEVIATH_LIVE_OPENROUTER_IMAGES").as_deref() != Ok("1") {
        eprintln!("LEVIATH_LIVE_OPENROUTER_IMAGES is not 1; skipping");
        return;
    }
    let key = std::env::var("OPENROUTER_API_KEY").expect("OPENROUTER_API_KEY is set");
    let model = std::env::var("LEVIATH_LIVE_OPENROUTER_IMAGE_MODEL")
        .unwrap_or_else(|_| "black-forest-labs/flux-3-image-klein".to_string());
    let gateway = OpenRouterProvider::new(reqwest::Client::new(), key);
    gateway
        .prime_capabilities()
        .await
        .expect("the model listing reads");
    let request = InferenceRequest {
        system: vec![],
        messages: vec![Message {
            role: "user".into(),
            content: MessageContent::Text(
                "A single small green leaf on a plain white background.".into(),
            ),
            cache_breakpoint: false,
            reasoning: None,
        }],
        model: model.clone(),
        max_tokens: 0,
        temperature: 0.0,
        tools: vec![],
        extra: serde_json::json!({ "output_mimes": ["image/png"], "aspect_ratio": "1:1" }),
        request_timeout_secs: Some(300),
    };
    let response = gateway.infer(&request).await.expect("a picture");
    println!("{}", response.content);
    println!(
        "reported cost: {:?} USD",
        response.tokens_used.reported_cost_usd
    );
    let picture = response.parts.first().expect("one part");
    println!("part type: {}", picture.mime_type.as_str());
    assert!(!picture.bytes.is_empty(), "an empty picture");
    // The stage asked for PNG: a model whose images listing names `png`
    // must answer PNG; any other may answer in its own default.
    let formats = gateway
        .learned_models()
        .and_then(|learned| learned.listed(&model))
        .and_then(|m| m.image_formats)
        .unwrap_or_default();
    println!("formats the images listing names for {model}: {formats:?}");
    if formats.iter().any(|f| f == "png") {
        assert_eq!(picture.mime_type.as_str(), "image/png");
    } else {
        assert!(
            picture.mime_type.as_str().starts_with("image/"),
            "{} is not an image",
            picture.mime_type.as_str()
        );
    }
}
