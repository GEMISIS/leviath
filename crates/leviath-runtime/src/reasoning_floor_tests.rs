//! Which models are told not to reason, which are asked for the least effort,
//! and how a refusal of "no reasoning" is recognised and remembered.

use super::*;
use leviath_providers::{
    FailureKind, InferenceResponse, LearnedModel, LearnedModels, ModelCapabilities,
};
use serde_json::json;

/// A provider that only has a name and, for a gateway, a catalogue.
struct Named {
    name: &'static str,
    learned: Option<LearnedModels>,
}

#[async_trait::async_trait]
impl Provider for Named {
    async fn infer(&self, _r: &InferenceRequest) -> leviath_providers::Result<InferenceResponse> {
        Err(ProviderError::Other("not called".into()))
    }
    async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _m: &str) -> usize {
        1
    }
    fn name(&self) -> &str {
        self.name
    }
    fn capabilities(&self, _m: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    fn learned_models(&self) -> Option<&LearnedModels> {
        self.learned.as_ref()
    }
}

/// One catalogue entry: `(id, reasoning.mandatory, supported_efforts)`.
type Entry<'a> = (&'a str, Option<bool>, Option<&'a [&'a str]>);

/// An OpenRouter provider whose catalogue lists `entries`.
fn gateway(entries: &[Entry<'_>]) -> Named {
    let learned = LearnedModels::default();
    learned.replace(
        entries
            .iter()
            .map(|(id, mandatory, efforts)| {
                (
                    id.to_string(),
                    LearnedModel {
                        reasoning_mandatory: *mandatory,
                        reasoning_efforts: efforts
                            .map(|e| e.iter().map(|l| l.to_string()).collect()),
                        ..LearnedModel::default()
                    },
                )
            })
            .collect(),
    );
    Named {
        name: "openrouter",
        learned: Some(learned),
    }
}

fn unread() -> Named {
    Named {
        name: "openrouter",
        learned: None,
    }
}

/// What `floor_reasoning` makes of OpenRouter's "no reasoning" switch.
fn floored(provider: &dyn Provider, model: &str) -> serde_json::Value {
    let mut extra = json!({ "reasoning": { "enabled": false } });
    floor_reasoning(provider, model, &mut extra);
    extra
}

const OFF: &str = r#"{"reasoning":{"enabled":false}}"#;

fn effort(level: &str) -> serde_json::Value {
    json!({ "reasoning": { "effort": level } })
}

#[test]
fn the_catalogue_decides_when_it_describes_the_model() {
    const SPARK: &[&str] = &["max", "xhigh", "high", "medium", "low", "minimal"];
    const PRO: &[&str] = &["xhigh", "high", "medium"];
    let provider = gateway(&[
        ("meta/muse-spark-1.3", Some(true), Some(SPARK)),
        ("openai/gpt-5.5-pro", Some(true), Some(PRO)),
        ("openai/gpt-5-pro", Some(true), Some(&["high"])),
        ("deepseek/deepseek-r1", Some(true), None),
        ("acme/odd-levels", Some(true), Some(&["none"])),
        ("qwen/qwen3.6-plus", Some(false), Some(&["low", "none"])),
        ("meta/muse-optional", Some(false), None),
    ]);
    // Muse Spark must reason and takes `low`.
    assert_eq!(floored(&provider, "meta/muse-spark-1.3"), effort("low"));
    // Found by its bare name too, the way the catalogue is searched elsewhere.
    assert_eq!(floored(&provider, "muse-spark-1.3"), effort("low"));
    // A model whose levels leave out `low` gets the least it names.
    assert_eq!(floored(&provider, "openai/gpt-5.5-pro"), effort("medium"));
    assert_eq!(floored(&provider, "openai/gpt-5-pro"), effort("high"));
    // No levels, or none that are levels: `low`.
    assert_eq!(floored(&provider, "deepseek/deepseek-r1"), effort("low"));
    assert_eq!(floored(&provider, "acme/odd-levels"), effort("low"));
    // A model that may be told not to reason is told so, even in a family
    // that usually must: the catalogue outranks the list.
    assert_eq!(floored(&provider, "qwen/qwen3.6-plus").to_string(), OFF);
    assert_eq!(floored(&provider, "meta/muse-optional").to_string(), OFF);
}

#[test]
fn the_family_list_answers_when_the_catalogue_cannot() {
    // Never read.
    assert_eq!(floored(&unread(), "meta/muse-spark-1.3"), effort("low"));
    assert_eq!(floored(&unread(), "qwen/qwen3.6-plus").to_string(), OFF);
    // Read, but silent on the model or on its reasoning.
    let provider = gateway(&[("meta/muse-spark-1.3", None, None)]);
    assert_eq!(floored(&provider, "meta/muse-spark-1.3"), effort("low"));
    assert_eq!(floored(&provider, "meta/muse-new"), effort("low"));
    assert_eq!(floored(&provider, "acme/plain").to_string(), OFF);
}

#[test]
fn only_a_no_reasoning_switch_is_touched() {
    let provider = unread();
    for extra in [
        json!(null),
        json!({ "think": false }),
        json!({ "reasoning": { "effort": "low" } }),
        json!({ "reasoning": { "enabled": true } }),
    ] {
        let mut floored = extra.clone();
        floor_reasoning(&provider, "meta/muse-spark-1.3", &mut floored);
        assert_eq!(floored, extra);
    }
}

#[test]
fn a_remembered_refusal_floors_that_model_from_then_on() {
    let provider = unread();
    let model = "acme/remembered-thinker";
    assert_eq!(floored(&provider, model).to_string(), OFF);
    remember_must_reason("openrouter", model);
    assert_eq!(floored(&provider, model), effort("low"));
    // Only for that provider and model.
    assert_eq!(floored(&provider, "acme/other-thinker").to_string(), OFF);
    let other = Named {
        name: "elsewhere",
        learned: None,
    };
    assert_eq!(floored(&other, model).to_string(), OFF);
}

/// What OpenRouter answered for Muse Spark on the run in the issue.
fn mandatory_refusal() -> ProviderError {
    ProviderError::ApiError(
        r#"[bad-request] HTTP 400 Bad Request: {"error":{"message":"Reasoning is mandatory for this endpoint and cannot be disabled.","code":400,"metadata":{"provider_name":null}}}"#
            .into(),
    )
}

/// A 400 whose body has "reasoning" and "mandatory" in it but is about
/// something else.
fn missing_field() -> ProviderError {
    ProviderError::labelled(
        FailureKind::BadRequest,
        "HTTP 400",
        "Invalid value for reasoning: max_tokens mandatory field missing",
    )
}

#[test]
fn a_refusal_of_no_reasoning_is_a_400_that_says_it_is_mandatory() {
    let spark = mandatory_refusal();
    assert_eq!(spark.failure_kind(), Some(FailureKind::BadRequest));
    assert!(refuses_reasoning_off(&spark));
    // A script provider's unlabelled message carries the status in its text.
    let script = ProviderError::ApiError("HTTP 400: reasoning is mandatory here".into());
    assert!(refuses_reasoning_off(&script));
    // A 400 about reasoning that is not this refusal, one about something
    // else, and a server error that says the same words.
    let budget = ProviderError::ApiError("HTTP 400: reasoning budget too large".into());
    assert!(!refuses_reasoning_off(&budget));
    // Both words, but not the refusal: a missing field of the reasoning object.
    assert!(!refuses_reasoning_off(&missing_field()));
    let other = ProviderError::labelled(FailureKind::BadRequest, "HTTP 400", "bad model");
    assert!(!refuses_reasoning_off(&other));
    let server = ProviderError::ApiError("HTTP 500: reasoning is mandatory".into());
    assert!(!refuses_reasoning_off(&server));
}

fn request(model: &str, extra: serde_json::Value) -> InferenceRequest {
    InferenceRequest {
        system: vec![],
        messages: vec![],
        model: model.to_string(),
        max_tokens: 10,
        temperature: 0.0,
        tools: vec![],
        extra,
        request_timeout_secs: None,
    }
}

#[test]
fn a_refused_switch_comes_back_floored_and_is_remembered() {
    let provider = unread();
    let model = "acme/refuses-in-a-test";
    let sent = request(
        model,
        json!({ "reasoning": { "enabled": false }, "usage": true }),
    );
    let again = after_refusal(&provider, &sent, &mandatory_refusal()).expect("asked again");
    assert_eq!(
        again.extra,
        json!({ "reasoning": { "effort": "low" }, "usage": true })
    );
    assert_eq!(again.model, model);
    // Remembered: the next request to it goes out floored from the start.
    assert_eq!(floored(&provider, model), effort("low"));
}

#[test]
fn any_other_failure_or_request_is_not_asked_again() {
    let provider = unread();
    let off = request(
        "acme/not-refused",
        json!({ "reasoning": { "enabled": false } }),
    );
    let other = ProviderError::ApiError("HTTP 400: bad model".into());
    assert!(after_refusal(&provider, &off, &other).is_none());
    // A 400 that names reasoning and a mandatory field is not the refusal
    // either: no second trip, and the model is not remembered.
    assert!(after_refusal(&provider, &off, &missing_field()).is_none());
    // Already floored: nothing left to take back.
    let low = request("acme/not-refused", effort("low"));
    assert!(after_refusal(&provider, &low, &mandatory_refusal()).is_none());
    // Neither was remembered.
    assert_eq!(floored(&provider, "acme/not-refused").to_string(), OFF);
}

/// OpenRouter in front of a model that always reasons and that its catalogue
/// says nothing about: it refuses "no reasoning" with the issue's 400 and
/// answers anything else. Every request's reasoning switch is recorded.
struct AlwaysThinks {
    sent: std::sync::Mutex<Vec<serde_json::Value>>,
}

#[async_trait::async_trait]
impl Provider for AlwaysThinks {
    async fn infer(&self, r: &InferenceRequest) -> leviath_providers::Result<InferenceResponse> {
        leviath_core::sync::lock(&self.sent).push(r.extra["reasoning"].clone());
        if turns_reasoning_off(&r.extra) {
            return Err(mandatory_refusal());
        }
        Ok(InferenceResponse {
            content: "Scaffold The Poly Wright".to_string(),
            tool_calls: vec![],
            tokens_used: leviath_providers::TokenUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                cached_tokens: 0,
                cache_write_tokens: 0,
                reported_cost_usd: None,
            },
            finish_reason: leviath_providers::FinishReason::Complete,
            reasoning: None,
            parts: Vec::new(),
            content_is_reasoning: false,
        })
    }
    async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _m: &str) -> usize {
        100_000
    }
    fn name(&self) -> &str {
        "openrouter"
    }
    fn capabilities(&self, _m: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// Drive one title call for `model` to its end, as the world does, and say
/// what it came to.
async fn title_call(
    provider: std::sync::Arc<AlwaysThinks>,
    model: &str,
) -> crate::title_bridge::TitleOutcome {
    let pools = crate::inference_pool::InferencePools::new(
        crate::inference_pool::InferencePoolConfig::new(),
    );
    let mut extra = json!({ "reasoning": { "enabled": false } });
    floor_reasoning(provider.as_ref(), model, &mut extra);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    crate::title_bridge::tests::run_title_job(
        crate::title_bridge::TitleJob {
            entity: bevy_ecs::entity::Entity::PLACEHOLDER,
            provider,
            provider_name: "openrouter".to_string(),
            model: model.to_string(),
            request: request(model, extra),
            permit: pools.try_acquire("openrouter", model).expect("free"),
        },
        // One attempt: the refusal is answered outside the retry budget, so
        // a title still lands on a schedule that would retry nothing.
        crate::inference_bridge::RetryPolicy {
            max_attempts: 1,
            ..crate::inference_bridge::RetryPolicy::default()
        },
        tx,
        std::sync::Arc::new(tokio::sync::Notify::new()),
    )
    .await;
    rx.recv().await.expect("an outcome is always reported")
}

#[tokio::test]
async fn a_title_call_refused_for_no_reasoning_asks_again_with_the_least() {
    let provider = std::sync::Arc::new(AlwaysThinks {
        sent: std::sync::Mutex::new(Vec::new()),
    });
    assert_eq!(provider.count_tokens("t", "m").await, 1);
    assert_eq!(provider.max_context_tokens("m"), 100_000);
    let _ = provider.capabilities("m");
    let model = "acme/title-thinker";
    let outcome = title_call(provider.clone(), model).await;
    assert_eq!(
        outcome.result.expect("the second trip answers"),
        "Scaffold The Poly Wright"
    );
    // The next title call to the model skips the refusal.
    let outcome = title_call(provider.clone(), model).await;
    assert!(outcome.result.is_ok());
    assert_eq!(
        *leviath_core::sync::lock(&provider.sent),
        vec![
            json!({ "enabled": false }),
            json!({ "effort": "low" }),
            json!({ "effort": "low" }),
        ]
    );
}
