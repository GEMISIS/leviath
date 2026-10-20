//! A stage's provider-neutral `effort`, spelled per request by the provider
//! that serves it.

use super::inference::{PriorCalls, build_request, with_effort};
use super::transition_choice::routing_request;
use crate::components::{ContextWindow, InferenceConfig};
use crate::pipeline::StageInference;
use leviath_core::{Effort, Region, RegionKind};
use leviath_providers::{InferenceRequest, Provider};
use std::sync::Arc;

/// A provider that spells an effort as `spelling(level)`, or takes none when
/// `spelling` is `None`.
struct Spells {
    spelling: Option<fn(&str) -> serde_json::Value>,
}

#[async_trait::async_trait]
impl Provider for Spells {
    async fn infer(
        &self,
        _r: &InferenceRequest,
    ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
        Err(leviath_providers::ProviderError::Other("unused".into()))
    }
    async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _m: &str) -> usize {
        100_000
    }
    fn name(&self) -> &str {
        "spells"
    }
    fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
        leviath_providers::ModelCapabilities {
            max_output_tokens: 4_000,
            ..Default::default()
        }
    }
    fn effort_extra(&self, _m: &str, effort: Effort) -> serde_json::Value {
        self.spelling
            .map_or(serde_json::Value::Null, |spell| spell(effort.as_str()))
    }
}

/// Claude's spelling.
fn claude(level: &str) -> serde_json::Value {
    serde_json::json!({ "output_config": { "effort": level } })
}

/// The Responses routes' and OpenRouter's spelling.
fn responses(level: &str) -> serde_json::Value {
    serde_json::json!({ "reasoning": { "effort": level } })
}

fn window() -> ContextWindow {
    let mut w = ContextWindow::new(10_000);
    w.add_region(Region::new(
        "conversation".to_string(),
        RegionKind::Pinned,
        1000,
    ));
    w
}

fn stage_on(provider: &str, model: &str) -> StageInference {
    StageInference {
        provider_name: provider.to_string(),
        model: model.to_string(),
        ..Default::default()
    }
}

fn low() -> InferenceConfig {
    InferenceConfig {
        effort: Some(Effort::Low),
        ..Default::default()
    }
}

fn extra_for(si: &StageInference, provider: Spells, cfg: &InferenceConfig) -> serde_json::Value {
    let provider: Arc<dyn Provider> = Arc::new(provider);
    let (request, _, _) = build_request(
        &window(),
        Some(cfg),
        si,
        &provider,
        "survey",
        0,
        PriorCalls::default(),
    );
    request.extra
}

/// The issue's case: a stage on Muse Spark with a Claude fallback and a
/// fallback that takes no effort. Each request carries only the serving
/// provider's own field.
#[test]
fn each_model_in_the_list_gets_only_its_own_providers_field() {
    let cfg = low();
    assert_eq!(
        extra_for(
            &stage_on("meta", "muse-spark"),
            Spells {
                spelling: Some(responses)
            },
            &cfg
        ),
        responses("low")
    );
    assert_eq!(
        extra_for(
            &stage_on("anthropic", "claude-sonnet-5"),
            Spells {
                spelling: Some(claude)
            },
            &cfg
        ),
        claude("low")
    );
    assert_eq!(
        extra_for(
            &stage_on("ollama", "llama"),
            Spells { spelling: None },
            &cfg
        ),
        serde_json::Value::Null
    );
}

/// A stage with no effort asks the provider for nothing.
#[test]
fn a_stage_without_an_effort_sends_none() {
    let extra = extra_for(
        &stage_on("anthropic", "claude-sonnet-5"),
        Spells {
            spelling: Some(claude),
        },
        &InferenceConfig::default(),
    );
    assert_eq!(extra, serde_json::Value::Null);
}

/// A key the stage set in its own parameters stays as the stage wrote it;
/// the effort fills only what the stage left unset.
#[test]
fn the_stages_own_parameter_wins_over_its_effort() {
    let mut params = serde_json::Map::new();
    params.insert(
        "reasoning".to_string(),
        serde_json::json!({ "max_tokens": 9 }),
    );
    params.insert("top_p".to_string(), serde_json::json!(0.5));
    let provider = Spells {
        spelling: Some(responses),
    };
    let merged = with_effort(params.clone(), &provider, "m", Some(Effort::High));
    assert_eq!(merged, params);
    let mut fresh = serde_json::Map::new();
    fresh.insert("top_p".to_string(), serde_json::json!(0.5));
    let merged = with_effort(fresh, &provider, "m", Some(Effort::High));
    assert_eq!(merged["reasoning"], serde_json::json!({ "effort": "high" }));
    assert_eq!(merged.len(), 2);
}

/// A routing call on a stage with an effort asks for exactly what the
/// stage's own calls asked for, so a provider that caches on the reasoning
/// settings (Anthropic) keeps serving the prefix from cache.
#[test]
fn a_routing_call_carries_the_stages_effort_unchanged() {
    let cfg = low();
    let si = stage_on("anthropic", "claude-sonnet-5");
    let provider: Arc<dyn Provider> = Arc::new(Spells {
        spelling: Some(claude),
    });
    let (stage_request, _, _) = build_request(
        &window(),
        Some(&cfg),
        &si,
        &provider,
        "survey",
        0,
        PriorCalls::default(),
    );
    let routing = routing_request(
        &window(),
        Some(&cfg),
        &si,
        &provider,
        "survey",
        0,
        PriorCalls::default(),
    );
    assert_eq!(routing.extra, stage_request.extra);
    assert_eq!(routing.extra, claude("low"));
}
