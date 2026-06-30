//! Streaming inference support.

use leviath_runtime::AgentEngine;
use tokio_stream::StreamExt;

use super::io::RunIO;

/// Stream inference output, collecting the full response.
/// Used only for tool-less interactive stages.
pub async fn stream_inference(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    tool_filter: Option<&[String]>,
    io: &mut dyn RunIO,
) -> anyhow::Result<leviath_providers::InferenceResponse> {
    use leviath_runtime::ContextWindow;

    let provider = engine
        .get_provider(provider_name)
        .ok_or_else(|| anyhow::anyhow!("Provider '{}' not registered", provider_name))?;

    let (messages, max_tokens) = {
        let window = engine
            .world()
            .get::<ContextWindow>(entity)
            .ok_or_else(|| anyhow::anyhow!("Entity has no ContextWindow"))?;

        let messages = window.assemble_messages();
        let remaining = window.max_tokens.saturating_sub(window.current_tokens);
        let max_tokens = remaining.min(4096);
        (messages, max_tokens)
    };

    // Tool-less streaming: always empty tools list
    let tools: Vec<leviath_providers::Tool> = Vec::new();
    let filtered_tools = if let Some(filter) = tool_filter {
        if filter.is_empty() {
            tools
        } else {
            tools
                .into_iter()
                .filter(|t| filter.iter().any(|f| f == &t.name))
                .collect()
        }
    } else {
        tools
    };

    // Respect each model's temperature support (e.g. claude-opus-4-8 deprecates it).
    let temperature = if provider.capabilities(model_name).supports_temperature {
        0.7
    } else {
        0.0
    };
    let request = leviath_providers::InferenceRequest {
        messages,
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

    if let Some(mut state) = engine
        .world_mut()
        .get_mut::<leviath_runtime::AgentState>(entity)
    {
        state.iteration += 1;
    }

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
    use leviath_core::{Blueprint, ContextLayout, RegionKind, Stage};
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

    fn make_blueprint() -> Blueprint {
        let layout = ContextLayout::new(
            vec![
                RegionDefinition::new("system".to_string(), RegionKind::Pinned, 2000),
                RegionDefinition::new(
                    "conversation".to_string(),
                    RegionKind::SlidingWindow { max_items: 50 },
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

    #[tokio::test]
    async fn stream_inference_captures_output() {
        let bp = make_blueprint();
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Streamed response");
        let mut io = MockIO::new();

        let response = stream_inference(&mut engine, entity, "mock", "test-model", None, &mut io)
            .await
            .unwrap();

        assert_eq!(response.content, "Streamed response");

        // Check that io captured the output
        let all_output: String = io.outputs.join("");
        assert!(
            all_output.contains("Assistant:"),
            "Expected 'Assistant:' prefix in output: {:?}",
            io.outputs
        );
        assert!(
            all_output.contains("Streamed response"),
            "Expected streamed content in output: {:?}",
            io.outputs
        );
    }

    #[tokio::test]
    async fn stream_inference_returns_token_usage() {
        let bp = make_blueprint();
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Test");
        let mut io = MockIO::new();

        let response = stream_inference(&mut engine, entity, "mock", "test-model", None, &mut io)
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
            &mut io,
        )
        .await;

        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("not registered"),
            "Expected 'not registered' error"
        );
    }

    #[tokio::test]
    async fn stream_inference_outputs_newline_at_end() {
        let bp = make_blueprint();
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Content");
        let mut io = MockIO::new();

        stream_inference(&mut engine, entity, "mock", "test-model", None, &mut io)
            .await
            .unwrap();

        // Last output should be a newline
        assert_eq!(
            io.outputs.last().map(|s| s.as_str()),
            Some("\n"),
            "Expected trailing newline in outputs: {:?}",
            io.outputs
        );
    }
}
