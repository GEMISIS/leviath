//! Streaming inference support.

use leviath_runtime::AgentEngine;
use tokio_stream::StreamExt;

/// Stream inference output directly to stdout, collecting the full response.
/// Used only for tool-less interactive stages.
pub async fn stream_inference(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    tool_filter: Option<&[String]>,
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

    print!("\nAssistant: ");
    use std::io::Write;

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(|e| anyhow::anyhow!("Stream chunk error: {}", e))?;

        if !chunk.delta.is_empty() {
            print!("{}", chunk.delta);
            std::io::stdout().flush().ok();
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

    println!();

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
