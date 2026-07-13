//! Streaming inference support.

use leviath_runtime::AgentEngine;
use tokio_stream::StreamExt;

use super::io::RunIO;

/// Stream inference output, collecting the full response.
/// Used for interactive stages — supports both tool-less and tool-bearing modes.
/// When `tools` is non-empty, the model may return tool calls in the response.
pub async fn stream_inference(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    tool_filter: Option<&[String]>,
    tools: &[leviath_providers::Tool],
    io: &mut dyn RunIO,
) -> anyhow::Result<leviath_providers::InferenceResponse> {
    use leviath_runtime::ContextWindow;

    let provider = engine
        .get_provider(provider_name)
        .ok_or_else(|| anyhow::anyhow!("Provider '{}' not registered", provider_name))?;

    let (assembled, max_tokens) = {
        let window = engine
            .world()
            .get::<ContextWindow>(entity)
            .expect("Entity always has ContextWindow — spawned via AgentPool");

        let assembled = window.assemble();
        let remaining = window.max_tokens.saturating_sub(window.current_tokens);
        let max_tokens = remaining.min(4096);
        (assembled, max_tokens)
    };

    // Filter tools based on tool_filter if provided
    let filtered_tools: Vec<leviath_providers::Tool> = if tools.is_empty() {
        Vec::new()
    } else if let Some(filter) = tool_filter {
        tools
            .iter()
            .filter(|t| filter.iter().any(|f| f == &t.name))
            .cloned()
            .collect()
    } else {
        tools.to_vec()
    };

    // Respect each model's temperature support (e.g. claude-opus-4-8 deprecates it).
    let temperature = if provider.capabilities(model_name).supports_temperature {
        0.7
    } else {
        0.0
    };
    let request = leviath_providers::InferenceRequest {
        messages: assembled.messages,
        system: assembled.system_blocks,
        model: model_name.to_string(),
        max_tokens,
        temperature,
        tools: filtered_tools,
        extra: serde_json::Value::Null,
    };

    let mut stream = provider
        .infer_stream(request)
        .await
        .map_err(|e| anyhow::anyhow!("Stream error: {}", e))?;

    let mut full_content = String::new();
    let mut final_tokens = None;
    let mut final_finish_reason = None;
    let mut all_tool_calls: Vec<leviath_providers::ToolCall> = Vec::new();

    io.on_output("\nAssistant: ").await;

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(|e| anyhow::anyhow!("Stream chunk error: {}", e))?;

        if !chunk.delta.is_empty() {
            io.on_output(&chunk.delta).await;
            full_content.push_str(&chunk.delta);
        }

        if let Some(tokens) = chunk.tokens {
            final_tokens = Some(tokens);
        }
        if let Some(reason) = chunk.finish_reason {
            final_finish_reason = Some(reason);
        }

        for tc_delta in &chunk.tool_calls {
            while all_tool_calls.len() <= tc_delta.index {
                all_tool_calls.push(leviath_providers::ToolCall {
                    id: String::new(),
                    name: String::new(),
                    arguments: serde_json::Value::Null,
                });
            }
            let tc = &mut all_tool_calls[tc_delta.index];
            if let Some(ref id) = tc_delta.id {
                tc.id.clone_from(id);
            }
            if let Some(ref name) = tc_delta.name {
                tc.name.clone_from(name);
            }
            if !tc_delta.arguments_delta.is_empty() && tc.arguments.is_null() {
                if let Ok(val) = serde_json::from_str(&tc_delta.arguments_delta) {
                    tc.arguments = val;
                }
            }
        }
    }

    io.on_output("\n").await;

    engine
        .world_mut()
        .get_mut::<leviath_runtime::AgentState>(entity)
        .expect("Entity always has AgentState — spawned via AgentPool")
        .iteration += 1;

    let tokens_used = final_tokens.unwrap_or(leviath_providers::TokenUsage {
        prompt_tokens: 0,
        completion_tokens: full_content.len() / 4,
        total_tokens: full_content.len() / 4,
        cached_tokens: 0,
        cache_write_tokens: 0,
    });

    Ok(leviath_providers::InferenceResponse {
        content: full_content,
        tool_calls: all_tool_calls,
        tokens_used,
        finish_reason: final_finish_reason.unwrap_or(leviath_providers::FinishReason::Complete),
    })
}

#[cfg(test)]
mod tests {
    use super::super::helpers::initialize_context_window;
    use super::super::io::mock::MockIO;
    use super::*;
    use async_trait::async_trait;
    use leviath_core::blueprint::ModelConfig;
    use leviath_core::layout::RegionDefinition;
    use leviath_core::{Blueprint, ContextLayout, EvictionStrategy, RegionKind, Stage};
    use leviath_providers::{
        FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, ModelInfo, Provider,
        ProviderError, TokenUsage,
    };
    use leviath_runtime::{AgentPool, ProviderRegistry};
    use std::sync::Arc;

    struct MockProvider {
        response_content: String,
    }

    impl MockProvider {
        fn new(content: &str) -> Self {
            Self {
                response_content: content.to_string(),
            }
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn infer(
            &self,
            _request: InferenceRequest,
        ) -> Result<InferenceResponse, ProviderError> {
            Ok(InferenceResponse {
                content: self.response_content.clone(),
                tool_calls: vec![],
                tokens_used: TokenUsage {
                    prompt_tokens: 20,
                    completion_tokens: 8,
                    total_tokens: 28,
                    cached_tokens: 3,
                    cache_write_tokens: 0,
                },
                finish_reason: FinishReason::Complete,
            })
        }

        fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len() / 4
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            100_000
        }

        fn name(&self) -> &str {
            "mock"
        }

        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(vec![])
        }
    }

    /// A provider whose `capabilities()` reports no temperature support, to
    /// exercise the `temperature = 0.0` branch in `stream_inference`.
    struct NoTemperatureProvider;

    #[async_trait]
    impl Provider for NoTemperatureProvider {
        async fn infer(
            &self,
            _request: InferenceRequest,
        ) -> Result<InferenceResponse, ProviderError> {
            Ok(InferenceResponse {
                content: "no-temp".to_string(),
                tool_calls: vec![],
                tokens_used: TokenUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                finish_reason: FinishReason::Complete,
            })
        }

        fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len() / 4
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            100_000
        }

        fn name(&self) -> &str {
            "no-temp"
        }

        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            ModelCapabilities {
                supports_temperature: false,
                ..ModelCapabilities::default()
            }
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(vec![])
        }
    }

    /// A provider whose `infer()` always errors, to exercise the "Stream
    /// error: {}" mapping in `stream_inference` (the default `infer_stream`
    /// wraps `infer()`, so an `infer()` error surfaces as an `infer_stream`
    /// error before any stream is ever produced).
    struct FailingInferProvider;

    #[async_trait]
    impl Provider for FailingInferProvider {
        async fn infer(
            &self,
            _request: InferenceRequest,
        ) -> Result<InferenceResponse, ProviderError> {
            Err(ProviderError::ApiError("boom".to_string()))
        }

        fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len() / 4
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            100_000
        }

        fn name(&self) -> &str {
            "failing-infer"
        }

        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(vec![])
        }
    }

    /// A provider that implements `infer_stream` directly (rather than
    /// relying on the default single-chunk wrapper around `infer()`) so
    /// tests can drive multiple chunks: empty/non-empty deltas, chunks
    /// missing `tokens`/`finish_reason`, and multi-index tool-call deltas
    /// with valid, invalid, and already-set arguments JSON.
    struct MultiChunkStreamProvider;

    #[async_trait]
    impl Provider for MultiChunkStreamProvider {
        async fn infer(
            &self,
            _request: InferenceRequest,
        ) -> Result<InferenceResponse, ProviderError> {
            Err(ProviderError::ApiError(
                "infer_stream is overridden; infer() should not be called".to_string(),
            ))
        }

        async fn infer_stream(
            &self,
            _request: InferenceRequest,
        ) -> Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<
                            Item = Result<leviath_providers::StreamChunk, ProviderError>,
                        > + Send,
                >,
            >,
            ProviderError,
        > {
            let chunks = vec![
                // Empty delta: exercises the `if !chunk.delta.is_empty()` false branch.
                // No tokens/finish_reason: exercises both `if let Some(...)` false branches.
                Ok(leviath_providers::StreamChunk {
                    delta: String::new(),
                    tool_calls: vec![],
                    tokens: None,
                    finish_reason: None,
                }),
                // Two tool-call deltas at indices 0 and 2 (skipping 1), forcing the
                // fill-forward `while` loop to push multiple placeholder entries.
                // Index 0's arguments parse successfully from valid JSON.
                Ok(leviath_providers::StreamChunk {
                    delta: "hello ".to_string(),
                    tool_calls: vec![
                        leviath_providers::ToolCallDelta {
                            index: 0,
                            id: Some("call-0".to_string()),
                            name: Some("read_file".to_string()),
                            arguments_delta: "{\"path\":\"a.txt\"}".to_string(),
                        },
                        leviath_providers::ToolCallDelta {
                            index: 2,
                            id: Some("call-2".to_string()),
                            name: Some("bash".to_string()),
                            arguments_delta: "not valid json".to_string(),
                        },
                    ],
                    tokens: None,
                    finish_reason: None,
                }),
                // Second delta for index 0: id/name omitted (None), and
                // arguments_delta non-empty but arguments already set from the
                // previous chunk -- exercises the `tc.arguments.is_null()`
                // false skip branch.
                Ok(leviath_providers::StreamChunk {
                    delta: "world".to_string(),
                    tool_calls: vec![leviath_providers::ToolCallDelta {
                        index: 0,
                        id: None,
                        name: None,
                        arguments_delta: "{\"path\":\"ignored.txt\"}".to_string(),
                    }],
                    tokens: Some(TokenUsage {
                        prompt_tokens: 7,
                        completion_tokens: 3,
                        total_tokens: 10,
                        cached_tokens: 0,
                        cache_write_tokens: 0,
                    }),
                    finish_reason: Some(FinishReason::Complete),
                }),
            ];
            Ok(Box::pin(tokio_stream::iter(chunks)))
        }

        fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len() / 4
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            100_000
        }

        fn name(&self) -> &str {
            "multi-chunk"
        }

        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(vec![])
        }
    }

    /// A provider whose stream yields a single `Err` item, to exercise the
    /// "Stream chunk error: {}" mapping inside the `while let` loop.
    struct ErrorChunkStreamProvider;

    #[async_trait]
    impl Provider for ErrorChunkStreamProvider {
        async fn infer(
            &self,
            _request: InferenceRequest,
        ) -> Result<InferenceResponse, ProviderError> {
            Err(ProviderError::ApiError(
                "infer_stream is overridden; infer() should not be called".to_string(),
            ))
        }

        async fn infer_stream(
            &self,
            _request: InferenceRequest,
        ) -> Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<
                            Item = Result<leviath_providers::StreamChunk, ProviderError>,
                        > + Send,
                >,
            >,
            ProviderError,
        > {
            let chunks: Vec<Result<leviath_providers::StreamChunk, ProviderError>> =
                vec![Err(ProviderError::InvalidResponse("bad chunk".to_string()))];
            Ok(Box::pin(tokio_stream::iter(chunks)))
        }

        fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len() / 4
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            100_000
        }

        fn name(&self) -> &str {
            "error-chunk"
        }

        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(vec![])
        }
    }

    fn make_blueprint() -> Blueprint {
        let layout = ContextLayout::new(
            vec![
                RegionDefinition::new("system".to_string(), RegionKind::Pinned, 2000),
                RegionDefinition::new(
                    "conversation".to_string(),
                    RegionKind::SlidingWindow {
                        max_items: 50,
                        eviction_strategy: EvictionStrategy::PerItem,
                    },
                    10000,
                ),
            ],
            12000,
        );
        let stage = Stage::new(
            "main".to_string(),
            ModelConfig::new("mock".to_string(), "test-model".to_string()),
        );
        Blueprint::new(
            "test".to_string(),
            "test agent".to_string(),
            vec![stage],
            layout,
        )
    }

    fn make_engine_and_entity(
        blueprint: &Blueprint,
        content: &str,
    ) -> (AgentEngine, AgentPool, bevy_ecs::prelude::Entity) {
        let mut registry = ProviderRegistry::new();
        registry.register("mock".to_string(), Arc::new(MockProvider::new(content)));
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(blueprint.clone());
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();
        initialize_context_window(&mut engine, entity, blueprint, "test task");
        (engine, pool, entity)
    }

    /// Like [`make_engine_and_entity`], but with an arbitrary provider
    /// registered under `provider_name` instead of always using `MockProvider`.
    fn make_engine_and_entity_with_provider(
        blueprint: &Blueprint,
        provider_name: &str,
        provider: Arc<dyn Provider>,
    ) -> (AgentEngine, AgentPool, bevy_ecs::prelude::Entity) {
        let mut registry = ProviderRegistry::new();
        registry.register(provider_name.to_string(), provider);
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(blueprint.clone());
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();
        initialize_context_window(&mut engine, entity, blueprint, "test task");
        (engine, pool, entity)
    }

    #[tokio::test]
    async fn stream_inference_captures_output() {
        let bp = make_blueprint();
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Streamed response");
        let mut io = MockIO::new();

        let response = stream_inference(
            &mut engine,
            entity,
            "mock",
            "test-model",
            None,
            &[],
            &mut io,
        )
        .await
        .unwrap();

        assert_eq!(response.content, "Streamed response");

        // Check that io captured the output
        let all_output: String = io.outputs.join("");
        assert!(all_output.contains("Assistant:"));
        assert!(all_output.contains("Streamed response"));
    }

    #[tokio::test]
    async fn stream_inference_returns_token_usage() {
        let bp = make_blueprint();
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Test");
        let mut io = MockIO::new();

        let response = stream_inference(
            &mut engine,
            entity,
            "mock",
            "test-model",
            None,
            &[],
            &mut io,
        )
        .await
        .unwrap();

        assert_eq!(response.tokens_used.prompt_tokens, 20);
        assert_eq!(response.tokens_used.completion_tokens, 8);
    }

    #[tokio::test]
    async fn stream_inference_missing_provider_errors() {
        let bp = make_blueprint();
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "unused");
        let mut io = MockIO::new();

        let result = stream_inference(
            &mut engine,
            entity,
            "nonexistent",
            "test-model",
            None,
            &[],
            &mut io,
        )
        .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not registered"));
    }

    #[tokio::test]
    async fn stream_inference_outputs_newline_at_end() {
        let bp = make_blueprint();
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Content");
        let mut io = MockIO::new();

        stream_inference(
            &mut engine,
            entity,
            "mock",
            "test-model",
            None,
            &[],
            &mut io,
        )
        .await
        .unwrap();

        // Last output should be a newline
        assert_eq!(io.outputs.last().map(|s| s.as_str()), Some("\n"));
    }

    #[tokio::test]
    async fn stream_inference_with_empty_tool_filter_yields_no_tools() {
        let bp = make_blueprint();
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "filtered");
        let mut io = MockIO::new();

        let filter: Vec<String> = vec![];
        let response = stream_inference(
            &mut engine,
            entity,
            "mock",
            "test-model",
            Some(&filter),
            &[],
            &mut io,
        )
        .await
        .unwrap();

        assert_eq!(response.content, "filtered");
    }

    #[tokio::test]
    async fn stream_inference_with_nonempty_tool_filter_yields_no_tools() {
        let bp = make_blueprint();
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "filtered");
        let mut io = MockIO::new();

        let filter = vec!["some_tool".to_string()];
        let response = stream_inference(
            &mut engine,
            entity,
            "mock",
            "test-model",
            Some(&filter),
            &[],
            &mut io,
        )
        .await
        .unwrap();

        assert_eq!(response.content, "filtered");
    }

    #[tokio::test]
    async fn stream_inference_without_temperature_support_uses_zero_temperature() {
        let bp = make_blueprint();
        let (mut engine, _pool, entity) =
            make_engine_and_entity_with_provider(&bp, "mock", Arc::new(NoTemperatureProvider));
        let mut io = MockIO::new();

        // No direct way to observe the request's temperature field from here,
        // but a successful round trip with this provider exercises the
        // `temperature = 0.0` branch (`capabilities().supports_temperature`
        // is false) rather than panicking or diverging.
        let response = stream_inference(
            &mut engine,
            entity,
            "mock",
            "test-model",
            None,
            &[],
            &mut io,
        )
        .await
        .unwrap();
        assert_eq!(response.content, "no-temp");
    }

    #[tokio::test]
    async fn stream_inference_infer_error_is_mapped_as_stream_error() {
        let bp = make_blueprint();
        let (mut engine, _pool, entity) =
            make_engine_and_entity_with_provider(&bp, "mock", Arc::new(FailingInferProvider));
        let mut io = MockIO::new();

        let result = stream_inference(
            &mut engine,
            entity,
            "mock",
            "test-model",
            None,
            &[],
            &mut io,
        )
        .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Stream error"));
    }

    #[tokio::test]
    async fn stream_inference_chunk_error_is_mapped_as_stream_chunk_error() {
        let bp = make_blueprint();
        let (mut engine, _pool, entity) =
            make_engine_and_entity_with_provider(&bp, "mock", Arc::new(ErrorChunkStreamProvider));
        let mut io = MockIO::new();

        let result = stream_inference(
            &mut engine,
            entity,
            "mock",
            "test-model",
            None,
            &[],
            &mut io,
        )
        .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Stream chunk error"));
    }

    #[tokio::test]
    async fn stream_inference_handles_multi_chunk_tool_call_deltas() {
        let bp = make_blueprint();
        let (mut engine, _pool, entity) =
            make_engine_and_entity_with_provider(&bp, "mock", Arc::new(MultiChunkStreamProvider));
        let mut io = MockIO::new();

        let response = stream_inference(
            &mut engine,
            entity,
            "mock",
            "test-model",
            None,
            &[],
            &mut io,
        )
        .await
        .unwrap();

        // Content accumulated only from non-empty deltas.
        assert_eq!(response.content, "hello world");

        // Three tool-call slots (indices 0, 1, 2) -- index 1 was never
        // targeted directly but must exist as a placeholder because index 2
        // was referenced first.
        assert_eq!(response.tool_calls.len(), 3);

        // Index 0: id/name from the first delta, arguments parsed from the
        // first delta's valid JSON and NOT overwritten by the second delta's
        // arguments_delta (since arguments was no longer null).
        assert_eq!(response.tool_calls[0].id, "call-0");
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert_eq!(
            response.tool_calls[0].arguments,
            serde_json::json!({"path": "a.txt"})
        );

        // Index 1: placeholder only, never targeted by any delta.
        assert_eq!(response.tool_calls[1].id, "");
        assert_eq!(response.tool_calls[1].name, "");
        assert!(response.tool_calls[1].arguments.is_null());

        // Index 2: invalid JSON arguments_delta silently fails to parse,
        // leaving arguments null.
        assert_eq!(response.tool_calls[2].id, "call-2");
        assert_eq!(response.tool_calls[2].name, "bash");
        assert!(response.tool_calls[2].arguments.is_null());

        // Final chunk's tokens/finish_reason become the response's.
        assert_eq!(response.tokens_used.prompt_tokens, 7);
        assert_eq!(response.finish_reason, FinishReason::Complete);
    }

    /// The mock providers above only exist to drive `stream_inference`
    /// through specific code paths via `infer`/`infer_stream`; the engine
    /// never calls their other trivial `Provider` trait methods along those
    /// paths, so cover them directly here rather than leave dead-looking
    /// (but harmless) test-only stubs.
    #[tokio::test]
    async fn mock_provider_trivial_trait_methods_are_well_formed() {
        let mock = MockProvider::new("x");
        assert_eq!(mock.count_tokens("abcd", "m"), 1);
        assert_eq!(mock.max_context_tokens("m"), 100_000);
        assert_eq!(mock.name(), "mock");
        assert!(mock.list_models().await.unwrap().is_empty());

        let no_temp = NoTemperatureProvider;
        assert_eq!(no_temp.count_tokens("abcd", "m"), 1);
        assert_eq!(no_temp.max_context_tokens("m"), 100_000);
        assert_eq!(no_temp.name(), "no-temp");
        assert!(no_temp.list_models().await.unwrap().is_empty());

        let failing = FailingInferProvider;
        assert_eq!(failing.count_tokens("abcd", "m"), 1);
        assert_eq!(failing.max_context_tokens("m"), 100_000);
        assert_eq!(failing.name(), "failing-infer");
        assert!(failing.list_models().await.unwrap().is_empty());

        let multi = MultiChunkStreamProvider;
        assert_eq!(multi.count_tokens("abcd", "m"), 1);
        assert_eq!(multi.max_context_tokens("m"), 100_000);
        assert_eq!(multi.name(), "multi-chunk");
        assert!(multi.list_models().await.unwrap().is_empty());
        // Cover the `infer()` fallback path (returns an error rather than panicking).
        assert!(multi
            .infer(InferenceRequest {
                system: vec![],
                messages: vec![],
                model: "m".to_string(),
                max_tokens: 1,
                temperature: 0.0,
                tools: vec![],
                extra: serde_json::Value::Null,
            })
            .await
            .is_err());

        let error_chunk = ErrorChunkStreamProvider;
        assert_eq!(error_chunk.count_tokens("abcd", "m"), 1);
        assert_eq!(error_chunk.max_context_tokens("m"), 100_000);
        assert_eq!(error_chunk.name(), "error-chunk");
        assert!(error_chunk.list_models().await.unwrap().is_empty());
        // Cover the `infer()` fallback path (returns an error rather than panicking).
        assert!(error_chunk
            .infer(InferenceRequest {
                system: vec![],
                messages: vec![],
                model: "m".to_string(),
                max_tokens: 1,
                temperature: 0.0,
                tools: vec![],
                extra: serde_json::Value::Null,
            })
            .await
            .is_err());
    }
}
