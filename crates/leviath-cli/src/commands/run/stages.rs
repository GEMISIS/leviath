//! Stage runner functions: interactive, autonomous, interactive_points.

use leviath_core::lifecycle::CompactionConfig;
use leviath_runtime::AgentEngine;

use crate::runstate::{self, RunMeta};

use super::helpers::record_stage_log;
use super::helpers::record_stage_output;
use super::inference::stream_inference;
use super::io::RunIO;

/// Run an interactive stage.
///
/// `run_context`: if `Some((run_id, meta))`, interaction is handled via the
/// file-based IPC channel (background worker). If `None`, stdin is used
/// (foreground) via `io.get_user_input()`.
#[allow(clippy::too_many_arguments)]
pub async fn run_interactive_stage<F, Fut>(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    max_iterations: usize,
    tools: &[leviath_providers::Tool],
    run_context: Option<(&str, &mut RunMeta)>,
    stage_name: &str,
    io: &mut dyn RunIO,
    executor: &mut F,
) -> anyhow::Result<()>
where
    F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut,
    Fut: std::future::Future<Output = Vec<(String, String)>>,
{
    use crate::interaction::{
        make_interaction_id, request_interaction_async, response_as_text, InteractionRequest,
    };
    use leviath_runtime::ContextWindow;

    let has_tools = !tools.is_empty();
    let mut turn = 0;

    // We need to hold the run_id separately since we consume run_context's meta
    // across iterations. Decouple them to avoid borrow issues.
    let (run_id_owned, meta_opt): (Option<String>, Option<&mut RunMeta>) = match run_context {
        Some((rid, m)) => (Some(rid.to_string()), Some(m)),
        None => (None, None),
    };

    // We need meta across loop iterations — box it optionally.
    let mut meta_holder = meta_opt;

    loop {
        if turn >= max_iterations {
            io.on_output("\n[Max turns reached]\n").await;
            break;
        }

        if has_tools {
            let per_turn_iters = 10_usize.min(max_iterations.saturating_sub(turn));
            let response = engine
                .run_inference_loop_filtered(
                    entity,
                    provider_name,
                    model_name,
                    tools.to_vec(),
                    per_turn_iters,
                    None,
                    None,
                    None,
                    executor,
                )
                .await
                .map_err(|e| anyhow::anyhow!("Inference error: {}", e))?;

            io.on_output(&format!("\nAssistant: {}", response.content))
                .await;
            io.on_tokens(
                response.tokens_used.prompt_tokens,
                response.tokens_used.completion_tokens,
                response.tokens_used.cached_tokens,
            )
            .await;

            // Route to per-stage files so the dashboard can display them.
            let token_line = format!(
                "[Tokens: {} in, {} out]",
                response.tokens_used.prompt_tokens, response.tokens_used.completion_tokens
            );
            if let (Some(run_id), Some(ref m)) = (&run_id_owned, &meta_holder) {
                record_stage_output(run_id, m.stage_index, &response.content);
                record_stage_log(run_id, m.stage_index, &token_line);
            }

            // Update meta token counts so the dashboard shows them before the
            // next interaction point (before they go to WaitingInput).
            if let Some(ref mut m) = meta_holder {
                m.prompt_tokens += response.tokens_used.prompt_tokens;
                m.completion_tokens += response.tokens_used.completion_tokens;
                m.cached_tokens += response.tokens_used.cached_tokens;
                m.touch();
                let _ = runstate::write_meta(m);
            }

            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                let tokens = response.content.len() / 4 + 1;
                let _ = window.add_to_region(
                    "conversation",
                    format!("Assistant: {}", response.content),
                    tokens,
                );
            }
        } else {
            let response =
                match stream_inference(engine, entity, provider_name, model_name, None, io).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::debug!("Streaming unavailable, falling back: {}", e);
                        let r = engine
                            .run_inference_filtered(
                                entity,
                                provider_name,
                                model_name,
                                Vec::new(),
                                None,
                            )
                            .await
                            .map_err(|e| anyhow::anyhow!("Inference error: {}", e))?;
                        io.on_output(&format!("\nAssistant: {}", r.content)).await;
                        r
                    }
                };

            io.on_tokens(
                response.tokens_used.prompt_tokens,
                response.tokens_used.completion_tokens,
                response.tokens_used.cached_tokens,
            )
            .await;

            if let Some(ref mut m) = meta_holder {
                m.prompt_tokens += response.tokens_used.prompt_tokens;
                m.completion_tokens += response.tokens_used.completion_tokens;
                m.cached_tokens += response.tokens_used.cached_tokens;
                m.touch();
                let _ = runstate::write_meta(m);
            }

            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                let tokens = response.content.len() / 4 + 1;
                let _ = window.add_to_region(
                    "conversation",
                    format!("Assistant: {}", response.content),
                    tokens,
                );
            }
        }

        // Build and dispatch the input request
        let req = InteractionRequest::free_text(
            make_interaction_id(0, turn),
            "Your response (leave empty or /quit to end):",
            stage_name,
            false, // not required — empty ends the loop
        );

        let input = if let (Some(run_id), Some(ref mut meta)) = (&run_id_owned, &mut meta_holder) {
            let resp = request_interaction_async(run_id, meta, req, None).await?;
            response_as_text(&resp)
        } else {
            // Foreground path: use RunIO for user input
            io.get_user_input("Your response (leave empty or /quit to end):")
                .await
                .unwrap_or_default()
        };

        if input.is_empty() || input == "/quit" || input == "/exit" {
            io.on_output("\n[Session ended]\n").await;
            break;
        }

        if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
            let tokens = input.len() / 4 + 1;
            let _ = window.add_to_region("conversation", format!("User: {}", input), tokens);
        }

        turn += 1;
    }

    Ok(())
}

/// Run an autonomous stage with the real tool executor.
#[allow(clippy::too_many_arguments)]
pub async fn run_autonomous_stage<F, Fut>(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    max_iterations: usize,
    tools: &[leviath_providers::Tool],
    routing: Option<&leviath_runtime::ToolResultRoutingConfig>,
    compaction_config: Option<&CompactionConfig>,
    io: &mut dyn RunIO,
    executor: &mut F,
) -> anyhow::Result<()>
where
    F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut,
    Fut: std::future::Future<Output = Vec<(String, String)>>,
{
    let response = engine
        .run_inference_loop_filtered(
            entity,
            provider_name,
            model_name,
            tools.to_vec(),
            max_iterations,
            None,
            routing,
            compaction_config,
            executor,
        )
        .await;

    match response {
        Ok(resp) => {
            io.on_output(&resp.content).await;
            io.on_tokens(
                resp.tokens_used.prompt_tokens,
                resp.tokens_used.completion_tokens,
                resp.tokens_used.cached_tokens,
            )
            .await;
        }
        Err(e) => {
            io.on_error(&format!("Inference error: {}", e)).await;
        }
    }
    Ok(())
}

/// Run an InteractivePoints stage: autonomous iterations with pauses at each interaction point.
///
/// `run_context`: if `Some((run_id, meta))`, interaction is handled via the
/// file-based IPC channel (background worker). If `None`, stdin is used
/// (foreground).
#[allow(clippy::too_many_arguments)]
pub async fn run_interactive_points_stage<F, Fut>(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    max_iterations: usize,
    tools: &[leviath_providers::Tool],
    routing: Option<&leviath_runtime::ToolResultRoutingConfig>,
    compaction_config: Option<&CompactionConfig>,
    points: &[leviath_core::blueprint::InteractionPoint],
    run_context: Option<(&str, &mut RunMeta)>,
    io: &mut dyn RunIO,
    executor: &mut F,
) -> anyhow::Result<()>
where
    F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut,
    Fut: std::future::Future<Output = Vec<(String, String)>>,
{
    use crate::interaction::{
        make_interaction_id, request_interaction_async, request_interaction_stdin,
        response_as_choice, response_as_text, InteractionRequest,
    };
    use leviath_runtime::ContextWindow;

    if points.is_empty() {
        return run_autonomous_stage(
            engine,
            entity,
            provider_name,
            model_name,
            max_iterations,
            tools,
            routing,
            compaction_config,
            io,
            executor,
        )
        .await;
    }

    let (run_id_owned, mut meta_holder): (Option<String>, Option<&mut RunMeta>) = match run_context
    {
        Some((rid, m)) => (Some(rid.to_string()), Some(m)),
        None => (None, None),
    };

    let segments = points.len() + 1;
    let iterations_per_segment = max_iterations / segments;
    let mut remaining_iterations = max_iterations;

    for (pt_idx, point) in points.iter().enumerate() {
        let iters = iterations_per_segment.min(remaining_iterations);
        if iters > 0 {
            let response = engine
                .run_inference_loop_filtered(
                    entity,
                    provider_name,
                    model_name,
                    tools.to_vec(),
                    iters,
                    None,
                    routing,
                    compaction_config,
                    executor,
                )
                .await;

            if let Ok(resp) = response {
                if !resp.content.is_empty() {
                    io.on_output(&resp.content).await;
                    // Route agent response to the per-stage output file so the dashboard can display it
                    if let (Some(run_id), Some(ref m)) = (&run_id_owned, &meta_holder) {
                        record_stage_output(run_id, m.stage_index, &resp.content);
                    }
                }
                // Update token counts in meta so the dashboard shows them before WaitingInput
                if let Some(ref mut m) = meta_holder {
                    let token_line = format!(
                        "[Tokens: {} in, {} out]",
                        resp.tokens_used.prompt_tokens, resp.tokens_used.completion_tokens
                    );
                    record_stage_log(&m.run_id, m.stage_index, &token_line);
                    m.prompt_tokens += resp.tokens_used.prompt_tokens;
                    m.completion_tokens += resp.tokens_used.completion_tokens;
                    m.cached_tokens += resp.tokens_used.cached_tokens;
                    m.touch();
                    let _ = runstate::write_meta(m);
                }
            }
            remaining_iterations = remaining_iterations.saturating_sub(iters);
        }

        // Build the interaction request with the right style / options
        let req_id = make_interaction_id(pt_idx, 0);
        let bp_style = &point.style;
        let ipc_req = match bp_style {
            leviath_core::blueprint::InteractionStyle::MultipleChoice => {
                InteractionRequest::multiple_choice(
                    req_id,
                    &point.prompt,
                    point.options.clone(),
                    &point.name,
                )
            }
            leviath_core::blueprint::InteractionStyle::Confirm => {
                InteractionRequest::confirm(req_id, &point.prompt, &point.name)
            }
            leviath_core::blueprint::InteractionStyle::FreeText => {
                InteractionRequest::free_text(req_id, &point.prompt, &point.name, point.required)
            }
        };

        // Dispatch via file IPC or stdin
        let user_text =
            if let (Some(run_id), Some(ref mut meta)) = (&run_id_owned, &mut meta_holder) {
                let resp = request_interaction_async(run_id, meta, ipc_req.clone(), None).await?;
                match bp_style {
                    leviath_core::blueprint::InteractionStyle::MultipleChoice
                    | leviath_core::blueprint::InteractionStyle::Confirm => {
                        // Resolve choice index → option string
                        response_as_choice(&resp, &ipc_req.options)
                            .cloned()
                            .unwrap_or_else(|| response_as_text(&resp))
                    }
                    leviath_core::blueprint::InteractionStyle::FreeText => response_as_text(&resp),
                }
            } else {
                // Foreground (stdin) path — `request_interaction_stdin` prints and reads
                let resp = request_interaction_stdin(&ipc_req);
                match bp_style {
                    leviath_core::blueprint::InteractionStyle::MultipleChoice
                    | leviath_core::blueprint::InteractionStyle::Confirm => {
                        response_as_choice(&resp, &ipc_req.options)
                            .cloned()
                            .unwrap_or_else(|| response_as_text(&resp))
                    }
                    leviath_core::blueprint::InteractionStyle::FreeText => response_as_text(&resp),
                }
            };

        if !user_text.is_empty() {
            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                let tokens = user_text.len() / 4 + 1;
                let content = format!("User [{}]: {}", point.name, user_text);
                let _ = window.add_to_region("conversation", content, tokens);
            }
        }
    }

    if remaining_iterations > 0 {
        let response = engine
            .run_inference_loop_filtered(
                entity,
                provider_name,
                model_name,
                tools.to_vec(),
                remaining_iterations,
                None,
                routing,
                compaction_config,
                executor,
            )
            .await;

        if let Ok(resp) = response {
            if !resp.content.is_empty() {
                io.on_output(&resp.content).await;
                if let (Some(run_id), Some(ref m)) = (&run_id_owned, &meta_holder) {
                    record_stage_output(run_id, m.stage_index, &resp.content);
                }
            }
            io.on_tokens(
                resp.tokens_used.prompt_tokens,
                resp.tokens_used.completion_tokens,
                resp.tokens_used.cached_tokens,
            )
            .await;
            if let (Some(_), Some(ref m)) = (&run_id_owned, &meta_holder) {
                let token_line = format!(
                    "[Tokens used: {} input, {} output]",
                    resp.tokens_used.prompt_tokens, resp.tokens_used.completion_tokens
                );
                record_stage_log(&m.run_id, m.stage_index, &token_line);
            }
        }
    }

    Ok(())
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

    /// A mock provider that returns canned responses for testing.
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
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cached_tokens: 2,
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

    fn make_blueprint(stages: Vec<Stage>) -> Blueprint {
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
        Blueprint::new("test".to_string(), "test agent".to_string(), stages, layout)
    }

    fn make_stage(name: &str) -> Stage {
        Stage::new(
            name.to_string(),
            ModelConfig::new("mock".to_string(), "test-model".to_string()),
        )
    }

    fn make_engine_and_entity(
        blueprint: &Blueprint,
        provider_content: &str,
    ) -> (
        leviath_runtime::AgentEngine,
        AgentPool,
        bevy_ecs::prelude::Entity,
    ) {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::new(provider_content)),
        );
        let mut engine = leviath_runtime::AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(blueprint.clone());
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();
        initialize_context_window(&mut engine, entity, blueprint, "test task");
        (engine, pool, entity)
    }

    fn noop_exec(
        _calls: Vec<leviath_providers::ToolCall>,
    ) -> std::future::Ready<Vec<(String, String)>> {
        std::future::ready(vec![])
    }

    // ─── Tests ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn interactive_stage_max_turns_outputs_message() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Hello from assistant");
        let mut io = MockIO::new();

        // max_iterations=0 means it immediately hits the limit
        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            0, // max_iterations
            &[],
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert!(
            io.outputs.iter().any(|o| o.contains("[Max turns reached]")),
            "Expected max turns message in outputs: {:?}",
            io.outputs
        );
    }

    #[tokio::test]
    async fn interactive_stage_quit_ends_session() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Hello from assistant");
        let mut io = MockIO::new().with_inputs(vec!["/quit".to_string()]);

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            10,
            &[], // no tools → uses stream_inference path
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert!(
            io.outputs.iter().any(|o| o.contains("[Session ended]")),
            "Expected session ended message in outputs: {:?}",
            io.outputs
        );
    }

    #[tokio::test]
    async fn interactive_stage_empty_input_ends_session() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Hi");
        // MockIO returns None when inputs are exhausted → unwrap_or_default → empty string → quit
        let mut io = MockIO::new();

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            10,
            &[],
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert!(
            io.outputs.iter().any(|o| o.contains("[Session ended]")),
            "Expected session ended in outputs: {:?}",
            io.outputs
        );
    }

    #[tokio::test]
    async fn interactive_stage_streams_assistant_output() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Test response content");
        let mut io = MockIO::new().with_inputs(vec!["/quit".to_string()]);

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            10,
            &[],
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        // The streaming path outputs the response content via io
        let all_output: String = io.outputs.join("");
        assert!(
            all_output.contains("Test response content"),
            "Expected assistant output in: {:?}",
            io.outputs
        );
    }

    #[tokio::test]
    async fn interactive_stage_reports_tokens() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Response");
        let mut io = MockIO::new().with_inputs(vec!["/quit".to_string()]);

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            10,
            &[],
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        // Token records should have been reported
        assert!(
            !io.token_records.is_empty(),
            "Expected token records to be reported"
        );
    }

    #[tokio::test]
    async fn autonomous_stage_outputs_response() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Autonomous result");
        let mut io = MockIO::new();

        run_autonomous_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            1,
            &[],
            None,
            None,
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        let all_output: String = io.outputs.join("");
        assert!(
            all_output.contains("Autonomous result"),
            "Expected response content in outputs: {:?}",
            io.outputs
        );
    }

    #[tokio::test]
    async fn autonomous_stage_reports_tokens() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Response");
        let mut io = MockIO::new();

        run_autonomous_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            1,
            &[],
            None,
            None,
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert_eq!(io.token_records.len(), 1);
        let (prompt, completion, cached) = io.token_records[0];
        assert_eq!(prompt, 10);
        assert_eq!(completion, 5);
        assert_eq!(cached, 2);
    }

    #[tokio::test]
    async fn autonomous_stage_error_uses_io() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "unused");
        let mut io = MockIO::new();

        run_autonomous_stage(
            &mut engine,
            entity,
            "nonexistent", // provider doesn't exist
            "test-model",
            1,
            &[],
            None,
            None,
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert!(
            io.errors.iter().any(|e| e.contains("Inference error")),
            "Expected inference error in errors: {:?}",
            io.errors
        );
    }

    #[tokio::test]
    async fn interactive_points_empty_delegates_to_autonomous() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Points result");
        let mut io = MockIO::new();

        // Empty points → delegates to run_autonomous_stage
        run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            1,
            &[],
            None,
            None,
            &[], // empty points
            None,
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        let all_output: String = io.outputs.join("");
        assert!(
            all_output.contains("Points result"),
            "Expected autonomous output: {:?}",
            io.outputs
        );
    }

    #[tokio::test]
    async fn interactive_stage_with_tools_works() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Tool response");
        let mut io = MockIO::new().with_inputs(vec!["/quit".to_string()]);

        // Provide a tool so the tool-path is taken
        let tools = vec![leviath_providers::Tool {
            name: "test_tool".to_string(),
            description: "a test tool".to_string(),
            parameters: serde_json::json!({}),
        }];

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            10,
            &tools,
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        // Should have gotten output from the tool-path
        let all_output: String = io.outputs.join("");
        assert!(
            all_output.contains("Tool response"),
            "Expected tool response in outputs: {:?}",
            io.outputs
        );
    }

    #[tokio::test]
    async fn interactive_stage_exit_ends_session() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Response");
        let mut io = MockIO::new().with_inputs(vec!["/exit".to_string()]);

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            10,
            &[],
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert!(
            io.outputs.iter().any(|o| o.contains("[Session ended]")),
            "Expected session ended message: {:?}",
            io.outputs
        );
    }

    #[tokio::test]
    async fn autonomous_stage_no_errors() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "OK");
        let mut io = MockIO::new();

        let result = run_autonomous_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            5,
            &[],
            None,
            None,
            &mut io,
            &mut noop_exec,
        )
        .await;

        assert!(result.is_ok());
        assert!(io.errors.is_empty());
    }

    #[tokio::test]
    async fn interactive_stage_multiple_turns() {
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Reply");
        let mut io =
            MockIO::new().with_inputs(vec!["first question".to_string(), "/quit".to_string()]);

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            10,
            &[],
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        // Should have multiple outputs (at least 2: response + session ended)
        assert!(
            io.outputs.len() >= 2,
            "Expected at least 2 outputs: {:?}",
            io.outputs
        );
    }

    // ─── run_interactive_points_stage: with actual interaction points ──────

    fn make_free_text_point(name: &str, prompt: &str) -> leviath_core::blueprint::InteractionPoint {
        leviath_core::blueprint::InteractionPoint {
            name: name.to_string(),
            prompt: prompt.to_string(),
            required: false,
            style: leviath_core::blueprint::InteractionStyle::FreeText,
            options: vec![],
        }
    }

    fn make_multiple_choice_point(
        name: &str,
        prompt: &str,
        options: Vec<String>,
    ) -> leviath_core::blueprint::InteractionPoint {
        leviath_core::blueprint::InteractionPoint {
            name: name.to_string(),
            prompt: prompt.to_string(),
            required: false,
            style: leviath_core::blueprint::InteractionStyle::MultipleChoice,
            options,
        }
    }

    fn make_confirm_point(
        name: &str,
        prompt: &str,
    ) -> leviath_core::blueprint::InteractionPoint {
        leviath_core::blueprint::InteractionPoint {
            name: name.to_string(),
            prompt: prompt.to_string(),
            required: false,
            style: leviath_core::blueprint::InteractionStyle::Confirm,
            options: vec![],
        }
    }

    /// Helper: spawn a background task that watches for pending.json and writes a response.
    /// Returns a JoinHandle that should be awaited or aborted after the test.
    fn spawn_interaction_responder(
        run_id: String,
        responses: Vec<crate::interaction::InteractionResponse>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut resp_iter = responses.into_iter();
            let mut last_req_id = String::new();
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                if let Some(req) = crate::interaction::read_request(&run_id) {
                    // Skip if this is the same request we already responded to
                    // (waiting for the main task to consume it)
                    if req.id == last_req_id {
                        continue;
                    }
                    if let Some(mut resp) = resp_iter.next() {
                        last_req_id = req.id.clone();
                        resp.request_id = req.id.clone();
                        crate::interaction::write_response(&run_id, &resp).unwrap();
                    } else {
                        break;
                    }
                }
            }
        })
    }

    #[tokio::test]
    async fn interactive_points_single_free_text_point_stdin() {
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent answer");
        let mut io = MockIO::new();

        let run_id = format!("test-ip-ft-{}", std::process::id());
        let mut meta = RunMeta::new(
            run_id.clone(), "test".into(), "/p".into(), "t".into(), None, "/tmp".into(), 1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![make_free_text_point("feedback", "What do you think?")];

        let responder = spawn_interaction_responder(
            run_id.clone(),
            vec![InteractionResponse::text("", "my feedback")],
        );

        run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            4,
            &[],
            None,
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        responder.abort();
        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn interactive_points_multiple_choice_stdin() {
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let run_id = format!("test-ip-mc-{}", std::process::id());
        let mut meta = RunMeta::new(
            run_id.clone(), "test".into(), "/p".into(), "t".into(), None, "/tmp".into(), 1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![make_multiple_choice_point(
            "pick_one",
            "Choose an option",
            vec!["Option A".to_string(), "Option B".to_string()],
        )];

        // choice_index 1 = "Option B" (0-based)
        let responder = spawn_interaction_responder(
            run_id.clone(),
            vec![InteractionResponse::choice("", 1)],
        );

        run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            4,
            &[],
            None,
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        responder.abort();
        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn interactive_points_confirm_stdin() {
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Agent response");
        let mut io = MockIO::new();

        let run_id = format!("test-ip-cf-{}", std::process::id());
        let mut meta = RunMeta::new(
            run_id.clone(), "test".into(), "/p".into(), "t".into(), None, "/tmp".into(), 1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![make_confirm_point("confirm_step", "Are you sure?")];

        let responder = spawn_interaction_responder(
            run_id.clone(),
            vec![InteractionResponse::text("", "yes")],
        );

        run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            4,
            &[],
            None,
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        responder.abort();
        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn interactive_points_multiple_points_all_visited() {
        use crate::interaction::InteractionResponse;

        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Mid-stage response");
        let mut io = MockIO::new();

        let run_id = format!("test-ip-mp-{}", std::process::id());
        let mut meta = RunMeta::new(
            run_id.clone(), "test".into(), "/p".into(), "t".into(), None, "/tmp".into(), 1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![
            make_free_text_point("step1", "Tell me about step 1"),
            make_multiple_choice_point(
                "step2",
                "Pick one",
                vec!["A".to_string(), "B".to_string()],
            ),
            make_confirm_point("step3", "Confirm?"),
        ];

        let responder = spawn_interaction_responder(
            run_id.clone(),
            vec![
                InteractionResponse::text("", "first input"),
                InteractionResponse::choice("", 0),
                InteractionResponse::text("", "yes"),
            ],
        );

        run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            9,
            &[],
            None,
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        responder.abort();
        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn interactive_points_with_zero_remaining_iterations() {
        use crate::interaction::InteractionResponse;

        // max_iterations = 1, points = 2 → iterations_per_segment rounds down to 0
        // The stage should still run (just skips inference) and ask interaction points
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Response");
        let mut io = MockIO::new();

        let run_id = format!("test-ip-zero-{}", std::process::id());
        let mut meta = RunMeta::new(
            run_id.clone(), "test".into(), "/p".into(), "t".into(), None, "/tmp".into(), 1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![
            make_free_text_point("p1", "Point 1"),
            make_free_text_point("p2", "Point 2"),
        ];

        let responder = spawn_interaction_responder(
            run_id.clone(),
            vec![
                InteractionResponse::text("", "a"),
                InteractionResponse::text("", "b"),
            ],
        );

        run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            1, // small max_iterations
            &[],
            None,
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        responder.abort();
        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn interactive_points_empty_user_input_is_ok() {
        use crate::interaction::InteractionResponse;

        // Empty answer → nothing injected into context window (branch coverage)
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Response");
        let mut io = MockIO::new();

        let run_id = format!("test-ip-empty-{}", std::process::id());
        let mut meta = RunMeta::new(
            run_id.clone(), "test".into(), "/p".into(), "t".into(), None, "/tmp".into(), 1,
        );
        runstate::create_run(&meta).unwrap();

        let points = vec![make_free_text_point("ask", "Say something")];

        // Respond with empty text
        let responder = spawn_interaction_responder(
            run_id.clone(),
            vec![InteractionResponse::text("", "")],
        );

        run_interactive_points_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            2,
            &[],
            None,
            None,
            &points,
            Some((&run_id, &mut meta)),
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        responder.abort();
        let _ = std::fs::remove_dir_all(runstate::run_dir(&run_id));
    }

    #[tokio::test]
    async fn autonomous_stage_with_tools() {
        // Test the autonomous stage with tools provided (exercises tool executor path)
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Tool result response");
        let mut io = MockIO::new();

        let tools = vec![leviath_providers::Tool {
            name: "my_tool".to_string(),
            description: "a test tool".to_string(),
            parameters: serde_json::json!({}),
        }];

        run_autonomous_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            1,
            &tools,
            None,
            None,
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        let all_output = io.outputs.join("");
        assert!(
            all_output.contains("Tool result response"),
            "Expected tool response in: {:?}",
            io.outputs
        );
    }

    #[tokio::test]
    async fn interactive_stage_with_tools_max_turns_reached() {
        // Test that the interactive stage tool-path also respects max_iterations=0
        let bp = make_blueprint(vec![make_stage("main")]);
        let (mut engine, _pool, entity) = make_engine_and_entity(&bp, "Response");
        let mut io = MockIO::new();

        let tools = vec![leviath_providers::Tool {
            name: "test_tool".to_string(),
            description: "test".to_string(),
            parameters: serde_json::json!({}),
        }];

        run_interactive_stage(
            &mut engine,
            entity,
            "mock",
            "test-model",
            0, // immediately hits limit
            &tools,
            None,
            "main",
            &mut io,
            &mut noop_exec,
        )
        .await
        .unwrap();

        assert!(
            io.outputs.iter().any(|o| o.contains("[Max turns reached]")),
            "Expected max turns message: {:?}",
            io.outputs
        );
    }
}
