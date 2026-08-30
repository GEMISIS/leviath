//! A configurable fake `Provider` and the value shapes this crate's tests
//! build by hand, each written once.
//!
//! These live here rather than in `leviath-testkit` on purpose: `leviath-core`
//! and `leviath-providers` dev-depend on testkit, so a testkit that depended
//! on them would close a dev-dependency cycle, and that cycle makes rustc see
//! two copies of `leviath_core` inside core's own test build (it stopped
//! core's tests compiling when tried). Every consumer of these is in this
//! crate, so nothing is lost by keeping them crate-local.

use async_trait::async_trait;
use leviath_providers::{
    InferenceRequest, InferenceResponse, ModelCapabilities, Provider, ProviderError, Result,
};

/// A `Provider` that does exactly what a test tells it to and nothing else.
///
/// Four daemon test modules each carried a unit-struct fake whose `infer`
/// failed with a fixed message, counted every text as one token and answered
/// a fixed context window. They were one shape with different constants, so
/// the constants are builder methods here.
#[derive(Clone, Debug)]
pub(crate) struct FakeProvider {
    name: String,
    reply: std::result::Result<String, String>,
    models: Vec<String>,
    context_window: usize,
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeProvider {
    /// Named `fake`, failing every `infer` with `test`, serving no models
    /// beyond what the default capability table says, with a 1000-token
    /// context window.
    pub(crate) fn new() -> Self {
        Self {
            name: "fake".to_string(),
            reply: Err("test".to_string()),
            models: Vec::new(),
            context_window: 1000,
        }
    }

    /// What [`Provider::name`] answers.
    pub(crate) fn named(mut self, name: &str) -> Self {
        self.name = name.to_string();
        self
    }

    /// Every `infer` succeeds with this text (see
    /// [`fixtures::inference_response`] for the rest of the response).
    pub(crate) fn replying(mut self, text: &str) -> Self {
        self.reply = Ok(text.to_string());
        self
    }

    /// Every `infer` fails with [`ProviderError::Other`] carrying this
    /// message.
    pub(crate) fn failing(mut self, error: &str) -> Self {
        self.reply = Err(error.to_string());
        self
    }

    /// Publish a served catalogue: [`Provider::serves_model`] answers a key
    /// that names one of these, and [`Provider::served_catalog`] lists them.
    pub(crate) fn serving(mut self, models: &[&str]) -> Self {
        self.models = models.iter().map(|m| (*m).to_string()).collect();
        self
    }

    /// What [`Provider::max_context_tokens`] answers for every model.
    pub(crate) fn context_window(mut self, tokens: usize) -> Self {
        self.context_window = tokens;
        self
    }
}

#[async_trait]
impl Provider for FakeProvider {
    async fn infer(&self, _request: &InferenceRequest) -> Result<InferenceResponse> {
        match &self.reply {
            Ok(text) => Ok(fixtures::inference_response(text)),
            Err(error) => Err(ProviderError::Other(error.clone())),
        }
    }

    async fn count_tokens(&self, _text: &str, _model: &str) -> usize {
        1
    }

    fn max_context_tokens(&self, _model: &str) -> usize {
        self.context_window
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }

    fn serves_model(&self, model_key: &str) -> Option<String> {
        if self.models.is_empty() {
            return self.serves_model_from_table(model_key);
        }
        self.models.iter().find(|m| *m == model_key).cloned()
    }

    fn served_catalog(&self) -> Option<Vec<String>> {
        (!self.models.is_empty()).then(|| self.models.clone())
    }
}

/// The value shapes the tests build by hand.
///
/// Every builder here returns exactly the literal it replaced, so a test
/// that asserted on a field of its own hand-built value asserts on the same
/// field of the same value now. A test that wants one field different uses
/// struct-update syntax over the fixture.
pub(crate) mod fixtures {
    use leviath_core::blueprint::{FanOutConfig, WorkerFailurePolicy};
    use leviath_core::run_meta::RunMeta;
    use leviath_providers::{FinishReason, InferenceRequest, InferenceResponse, TokenUsage};

    /// One prompt token, one completion token, nothing cached, no reported
    /// cost.
    pub(crate) fn token_usage() -> TokenUsage {
        TokenUsage {
            prompt_tokens: 1,
            completion_tokens: 1,
            total_tokens: 2,
            cached_tokens: 0,
            cache_write_tokens: 0,
            reported_cost_usd: None,
        }
    }

    /// An empty request for model `m` with a one-token budget: enough to
    /// hand to a provider whose answer does not depend on what was asked.
    pub(crate) fn inference_request() -> InferenceRequest {
        InferenceRequest {
            system: vec![],
            messages: vec![],
            model: "m".to_string(),
            max_tokens: 1,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        }
    }

    /// A complete text reply of `content` with no tool calls, costing
    /// [`token_usage`].
    pub(crate) fn inference_response(content: &str) -> InferenceResponse {
        InferenceResponse {
            content: content.to_string(),
            tool_calls: vec![],
            tokens_used: token_usage(),
            finish_reason: FinishReason::Complete,
            reasoning: None,
        }
    }

    /// A freshly started one-stage run of agent `a` (at `/p`) on task `t`
    /// in workdir `/w`, with no model recorded.
    pub(crate) fn run_meta(run_id: &str) -> RunMeta {
        RunMeta::new(
            run_id.to_string(),
            "a".to_string(),
            "/p".to_string(),
            "t".to_string(),
            None,
            "/w".to_string(),
            1,
        )
    }

    /// A single-worker fan-out with no worker source, whose split prompt is
    /// `s` and whose failed workers are skipped.
    pub(crate) fn fanout_config() -> FanOutConfig {
        FanOutConfig {
            worker_agent: None,
            worker_stage: None,
            worker_query: None,
            merge_stage: None,
            max_workers: 1,
            on_worker_failure: WorkerFailurePolicy::Continue,
            split_prompt: "s".to_string(),
            results_region: None,
            max_items: None,
            max_attempts: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_builder_reaches_every_answer() {
        let p = FakeProvider::default();
        assert_eq!(p.name(), "fake");
        assert_eq!(p.max_context_tokens("m"), 1000);
        assert_eq!(p.count_tokens("anything", "m").await, 1);
        assert!(p.served_catalog().is_none());
        // With no catalogue the default table decides, and it knows nothing
        // about this model.
        assert!(p.serves_model("nope").is_none());
        let err = p.infer(&fixtures::inference_request()).await.unwrap_err();
        assert!(err.to_string().contains("test"), "{err}");

        let p = FakeProvider::new()
            .named("mine")
            .replying("hi")
            .serving(&["m1", "m2"])
            .context_window(7);
        assert_eq!(p.name(), "mine");
        assert_eq!(p.max_context_tokens("m"), 7);
        assert_eq!(p.served_catalog(), Some(vec!["m1".into(), "m2".into()]));
        assert_eq!(p.serves_model("m2").as_deref(), Some("m2"));
        assert!(p.serves_model("nope").is_none());
        let reply = p.infer(&fixtures::inference_request()).await.unwrap();
        assert_eq!(reply.content, "hi");

        let err = FakeProvider::new()
            .failing("down")
            .infer(&fixtures::inference_request())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("down"), "{err}");
    }

    #[test]
    fn the_fixtures_are_the_literals_they_replaced() {
        assert_eq!(fixtures::token_usage().total_tokens, 2);
        assert_eq!(fixtures::inference_request().model, "m");
        assert_eq!(fixtures::inference_response("x").content, "x");
        let meta = fixtures::run_meta("r1");
        assert_eq!(
            (meta.run_id.as_str(), meta.agent_name.as_str()),
            ("r1", "a")
        );
        assert_eq!(fixtures::fanout_config().max_workers, 1);
    }
}
