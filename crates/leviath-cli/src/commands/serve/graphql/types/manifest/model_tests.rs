//! Tests for a stage's model block, and for the mirrors of those types.

use super::{
    MaxOutputTokens, MaxTokensContextPercent, MaxTokensCount, MaxTokensRegionPercent,
    ModelParameters, StageModelConfig, StageModelRoute,
};
use crate::commands::serve::graphql::filter::testkit::{exercise, exercise_list};
use crate::commands::serve::graphql::scalars::Json;

/// One route, as a manifest would write it.
fn route() -> StageModelRoute {
    StageModelRoute {
        provider: "anthropic".to_string(),
        model: "claude-sonnet-5".to_string(),
    }
}

/// Every function `#[mirror]` wrote for this file's types runs at least once.
///
/// A union is matched on, so it takes one value per variant: the generated
/// code has one arm each, and an arm nothing reaches is an arm nothing
/// measures.
#[tokio::test]
async fn every_mirrored_function_runs() {
    let routes = vec![route()];
    exercise(&routes).await;
    exercise_list(&routes).await;

    exercise(&[MaxTokensCount { tokens: 4000 }]).await;
    exercise(&[MaxTokensContextPercent { percent: 40.0 }]).await;
    exercise(&[MaxTokensRegionPercent {
        percent: 20.0,
        region: "notes".to_string(),
    }])
    .await;

    exercise(&[
        MaxOutputTokens::Count(MaxTokensCount { tokens: 4000 }),
        MaxOutputTokens::ContextPercent(MaxTokensContextPercent { percent: 40.0 }),
        MaxOutputTokens::RegionPercent(MaxTokensRegionPercent {
            percent: 20.0,
            region: "notes".to_string(),
        }),
    ])
    .await;

    exercise(&[ModelParameters {
        temperature: Some(0.2),
        max_output_tokens: Some(MaxOutputTokens::Count(MaxTokensCount { tokens: 4000 })),
        provider_params: Json(serde_json::json!({ "top_p": 0.9 })),
    }])
    .await;

    exercise(&[StageModelConfig {
        models: vec![route()],
        allow_user_default: false,
        parameters: ModelParameters {
            temperature: Some(0.2),
            max_output_tokens: None,
            provider_params: Json(serde_json::Value::Object(serde_json::Map::new())),
        },
        request_timeout_secs: Some(300),
    }])
    .await;
}

/// `from_table` pulls out the two named parameters and keeps everything else
/// in `providerParams` rather than dropping it.
#[test]
fn from_table_keeps_the_parameters_it_does_not_flatten() {
    let mut table = std::collections::HashMap::new();
    table.insert("temperature".to_string(), serde_json::json!(0.2));
    table.insert("max_output_tokens".to_string(), serde_json::json!("40%"));
    table.insert("top_p".to_string(), serde_json::json!(0.9));

    let parameters = ModelParameters::from_table(&table);
    assert_eq!(parameters.temperature, Some(0.2));
    let cap = parameters.max_output_tokens.expect("a relative cap parses");
    let MaxOutputTokens::ContextPercent(percent) = cap else {
        panic!("a percentage string parses as a context-percent cap");
    };
    assert_eq!(percent.percent, 40.0);
    assert_eq!(
        parameters.provider_params.0["top_p"],
        serde_json::json!(0.9)
    );
    assert!(parameters.provider_params.0.get("temperature").is_none());
    assert!(
        parameters
            .provider_params
            .0
            .get("max_output_tokens")
            .is_none()
    );
}

/// A cap that does not parse is left out rather than guessed at.
#[test]
fn from_table_drops_a_cap_that_does_not_parse() {
    let mut table = std::collections::HashMap::new();
    table.insert(
        "max_output_tokens".to_string(),
        serde_json::json!("nonsense"),
    );
    let parameters = ModelParameters::from_table(&table);
    assert!(parameters.max_output_tokens.is_none());
}

/// `StageModelConfig::from` carries the route list and the request timeout
/// straight through; `Stage.model` in `stage_tests.rs` is where these come
/// back through the schema rather than the bare conversion.
#[test]
fn a_model_config_carries_its_routes_and_timeout() {
    let mut core = leviath_core::blueprint::ModelConfig::new(
        "anthropic".to_string(),
        "claude-sonnet-5".to_string(),
    );
    core.request_timeout_secs = Some(300);
    let config = StageModelConfig::from(&core);
    assert_eq!(config.models[0].provider, "anthropic");
    assert_eq!(config.models[0].model, "claude-sonnet-5");
    assert!(config.allow_user_default);
    assert_eq!(config.request_timeout_secs, Some(300));
}
