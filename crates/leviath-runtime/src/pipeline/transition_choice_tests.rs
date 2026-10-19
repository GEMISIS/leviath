//! Which `tool_choice` a routing request carries, per provider and model, and
//! how a refusal of it is recognised and remembered; the reasoning control it
//! carries; and which replies give an answer.

use super::*;
use leviath_providers::{
    FailureKind, InferenceResponse, LearnedModel, LearnedModels, ModelCapabilities, ProviderError,
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

fn named(name: &'static str) -> Named {
    Named {
        name,
        learned: None,
    }
}

/// An OpenRouter provider whose catalogue lists `entries` as
/// `(id, supports_tool_choice)`.
fn gateway(entries: &[(&str, Option<bool>)]) -> Named {
    let learned = LearnedModels::default();
    learned.replace(
        entries
            .iter()
            .map(|(id, takes)| {
                (
                    id.to_string(),
                    LearnedModel {
                        supports_tool_choice: *takes,
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

#[test]
fn each_known_provider_gets_its_own_none_and_meta_gets_no_tools() {
    let none = Some(json!("none"));
    for provider in ["openai", "gemini", "codex", "xai", "grok"] {
        assert_eq!(
            routing_tool_choice(provider, &named(provider), "m-known"),
            none,
            "{provider}"
        );
    }
    assert_eq!(
        routing_tool_choice("anthropic", &named("anthropic"), "m-known"),
        Some(json!({ "type": "none" }))
    );
    // Meta's API takes only "auto", so the routing request clears its tools.
    assert_eq!(
        routing_tool_choice("meta", &named("meta"), "muse-spark-1.3"),
        None
    );
    assert_eq!(
        routing_tool_choice("someone-else", &named("x"), "m-known"),
        None
    );
}

#[test]
fn openrouter_decides_by_family_then_by_catalogue() {
    let none = Some(json!("none"));
    // No catalogue read: a known auto-only family is cleared, the rest are
    // sent "none".
    let unread = named("openrouter");
    assert_eq!(
        routing_tool_choice("openrouter", &unread, "meta/muse-spark-1.3"),
        None
    );
    assert_eq!(
        routing_tool_choice("openrouter", &unread, "qwen/qwen3.6-plus"),
        none
    );

    let listed = gateway(&[
        // OpenRouter lists `tool_choice` for Meta because "auto" works.
        ("meta/muse-spark-1.3", Some(true)),
        ("acme/no-choice", Some(false)),
        ("acme/takes-choice", Some(true)),
        ("acme/unsaid", None),
    ]);
    // The family wins over the listing, whichever spelling the stage used.
    assert_eq!(
        routing_tool_choice("openrouter", &listed, "meta/muse-spark-1.3"),
        None
    );
    assert_eq!(
        routing_tool_choice("openrouter", &listed, "muse-spark-1.3"),
        None
    );
    assert_eq!(
        routing_tool_choice("openrouter", &listed, "acme/no-choice"),
        None
    );
    assert_eq!(
        routing_tool_choice("openrouter", &listed, "acme/takes-choice"),
        none
    );
    // An entry that says nothing, and a model the listing does not carry.
    assert_eq!(
        routing_tool_choice("openrouter", &listed, "acme/unsaid"),
        none
    );
    assert_eq!(
        routing_tool_choice("openrouter", &listed, "acme/unlisted"),
        none
    );
}

#[test]
fn a_remembered_refusal_clears_the_tools_from_then_on() {
    let provider = named("openai");
    let model = "gpt-remembered-refusal";
    assert_eq!(
        routing_tool_choice("openai", &provider, model),
        Some(json!("none"))
    );
    remember_refused_tool_choice("openai", model);
    assert_eq!(routing_tool_choice("openai", &provider, model), None);
    // Only for that provider and model.
    assert_eq!(
        routing_tool_choice("openai", &provider, "gpt-other-model"),
        Some(json!("none"))
    );
}

#[test]
fn a_refusal_of_tool_choice_is_a_400_that_names_it() {
    // What Meta answered, through OpenRouter, on the run in the issue.
    let meta = ProviderError::ApiError(
        r#"[bad-request] HTTP 400 Bad Request: {"error":{"message":"Provider returned error","code":400,"metadata":{"raw":"{\"error\":{\"message\":\"only `\\\"auto\\\"` is supported for `tool_choice`\",\"param\":\"tool_choice\"}}","provider_name":"Meta"}}}"#
            .into(),
    );
    assert_eq!(meta.failure_kind(), Some(FailureKind::BadRequest));
    assert!(refuses_tool_choice(&meta));
    // A script provider's unlabelled message carries the status in its text.
    let script = ProviderError::ApiError("HTTP 400: param tool_choice is invalid".into());
    assert!(refuses_tool_choice(&script));
    // A 400 about something else, and a server error that names the field.
    let other = ProviderError::labelled(FailureKind::BadRequest, "HTTP 400", "bad model");
    assert!(!refuses_tool_choice(&other));
    let server = ProviderError::ApiError("HTTP 500: tool_choice crashed us".into());
    assert!(!refuses_tool_choice(&server));
}

#[test]
fn without_tool_choice_drops_the_tools_and_the_choice_only() {
    let request = InferenceRequest {
        system: vec![],
        messages: vec![],
        model: "m".to_string(),
        max_tokens: 10,
        temperature: 0.0,
        tools: vec![leviath_core::Tool {
            name: "read_file".to_string(),
            description: String::new(),
            parameters: json!({}),
        }],
        extra: json!({ "tool_choice": "none", "store": false }),
        request_timeout_secs: None,
    };
    let stripped = without_tool_choice(&request);
    assert!(stripped.tools.is_empty());
    assert_eq!(stripped.extra, json!({ "store": false }));
    assert_eq!(stripped.model, "m");
}

#[test]
fn routing_turns_reasoning_down_only_where_every_model_takes_it() {
    let low = json!({ "reasoning": { "effort": "low" } });
    for provider in ["meta", "xai", "grok"] {
        assert_eq!(
            routing_reasoning(provider, &named(provider), "m"),
            low,
            "{provider}"
        );
    }
    assert_eq!(
        routing_reasoning("codex", &named("codex"), "gpt-5.5"),
        json!({ "reasoning": { "effort": "low" }, "text": { "verbosity": "low" } })
    );
    // A change to the thinking settings costs Anthropic and Bedrock their
    // cached messages, and a model with nothing to turn down may refuse the
    // field, so these rely on the answer budget instead.
    for provider in ["anthropic", "bedrock", "ollama", "openai", "gemini"] {
        assert_eq!(
            routing_reasoning(provider, &named(provider), "m"),
            serde_json::Value::Null,
            "{provider}"
        );
    }
}

#[test]
fn openrouter_turns_reasoning_down_for_the_models_that_must_reason() {
    let low = json!({ "reasoning": { "effort": "low" } });
    let unread = named("openrouter");
    assert_eq!(
        routing_reasoning("openrouter", &unread, "meta/muse-spark-1.3"),
        low
    );
    // `enabled: false` is refused by a model that must reason, and a refused
    // routing call stops the run, so every other model gets nothing.
    assert_eq!(
        routing_reasoning("openrouter", &unread, "deepseek/deepseek-r1"),
        serde_json::Value::Null
    );
    // The catalogue's spelling decides, whichever one the stage used.
    let listed = gateway(&[("meta/muse-spark-1.3", Some(true))]);
    assert_eq!(
        routing_reasoning("openrouter", &listed, "muse-spark-1.3"),
        low
    );
    assert_eq!(
        routing_reasoning("openrouter", &unread, "muse-spark-1.3"),
        serde_json::Value::Null
    );
    // The catalogue's own word wins over the family list, with the least of
    // the levels it names: the title call's rule.
    let must = |efforts: &[&str]| LearnedModel {
        reasoning_mandatory: Some(true),
        reasoning_efforts: Some(efforts.iter().map(|e| e.to_string()).collect()),
        ..LearnedModel::default()
    };
    let learned = LearnedModels::default();
    learned.replace(
        [
            ("openai/gpt-5-pro".to_string(), must(&["high"])),
            (
                "anthropic/claude-opus-5.5".to_string(),
                must(&["low", "high"]),
            ),
            ("~anthropic/claude-sonnet-5.5".to_string(), must(&["low"])),
        ]
        .into_iter()
        .collect(),
    );
    let mandatory = Named {
        name: "openrouter",
        learned: Some(learned),
    };
    assert_eq!(
        routing_reasoning("openrouter", &mandatory, "openai/gpt-5-pro"),
        json!({ "reasoning": { "effort": "high" } })
    );
    // Anthropic's models must reason by the catalogue too, but the stage
    // request sent no effort, and a change to it costs Anthropic its cached
    // messages. They get nothing, under either id and either spelling.
    for model in [
        "anthropic/claude-opus-5.5",
        "claude-opus-5.5",
        "~anthropic/claude-sonnet-5.5",
    ] {
        assert_eq!(
            routing_reasoning("openrouter", &mandatory, model),
            serde_json::Value::Null,
            "{model}"
        );
    }
}

#[test]
fn a_cut_off_or_reasoning_reply_gives_no_routing_answer() {
    use leviath_providers::FinishReason::{Complete, Stop, TokenLimit};
    let reply = |finish: leviath_providers::FinishReason, from_reasoning: bool| InferenceResponse {
        content: "  summary\n".to_string(),
        tool_calls: vec![],
        tokens_used: leviath_providers::TokenUsage::new(1, 0, 0, 1),
        finish_reason: finish,
        reasoning: None,
        content_is_reasoning: from_reasoning,
        parts: Vec::new(),
    };
    assert_eq!(routing_answer(&reply(Complete, false)), Some("summary"));
    assert_eq!(routing_answer(&reply(Stop, false)), Some("summary"));
    assert_eq!(routing_answer(&reply(TokenLimit, false)), None);
    assert_eq!(routing_answer(&reply(Complete, true)), None);
    assert_eq!(routing_answer(&reply(TokenLimit, true)), None);
}
