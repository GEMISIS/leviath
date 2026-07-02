//! Agent execution engine using bevy_ecs.

use bevy_ecs::prelude::*;
use leviath_providers::{
    InferenceRequest, InferenceResponse, Message, Provider, ProviderError, Tool,
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::info;

use crate::components::{
    AgentMessage, AgentState, AgentStatus, CancellationToken, ContextWindow, InferenceResult,
    MessageInbox, NeedsCompaction,
};
use crate::systems;

/// Registry of LLM providers, keyed by provider name.
///
/// Used by the engine to look up the correct provider for each agent's
/// current stage based on its blueprint's ModelConfig.
#[derive(Clone)]
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    /// Create a new empty provider registry.
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    /// Register a provider by name.
    pub fn register(&mut self, name: String, provider: Arc<dyn Provider>) {
        self.providers.insert(name, provider);
    }

    /// Get a provider by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Provider>> {
        self.providers.get(name)
    }

    /// Check if a provider is registered.
    pub fn has(&self, name: &str) -> bool {
        self.providers.contains_key(name)
    }

    /// Get all registered provider names.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.keys().map(|k| k.as_str()).collect()
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The main agent execution engine.
///
/// Manages the ECS world, schedules systems, and drives agent execution
/// through a game-loop-inspired tick model.
pub struct AgentEngine {
    /// ECS world containing all agents and their components
    world: World,

    /// System schedule for executing agent behaviors
    schedule: Schedule,

    /// Provider registry for looking up LLM providers
    provider_registry: ProviderRegistry,

    /// Sender side of the message channel for sending messages to agents
    message_tx: mpsc::UnboundedSender<AgentMessage>,

    /// Receiver side of the message channel
    message_rx: mpsc::UnboundedReceiver<AgentMessage>,
}

impl AgentEngine {
    /// Create a new agent engine.
    pub fn new() -> Self {
        info!("Initializing Leviath agent engine");

        let world = World::new();
        let mut schedule = Schedule::default();

        schedule.add_systems((
            systems::context_management_system,
            systems::inference_system,
            systems::eviction_system,
            systems::pool_management_system,
            systems::child_completion_system,
            systems::cascade_kill_system,
            systems::stage_gating_system,
        ));

        let (message_tx, message_rx) = mpsc::unbounded_channel();

        Self {
            world,
            schedule,
            provider_registry: ProviderRegistry::new(),
            message_tx,
            message_rx,
        }
    }

    /// Create a new agent engine with a provider registry.
    pub fn with_providers(provider_registry: ProviderRegistry) -> Self {
        info!("Initializing Leviath agent engine with providers");

        let world = World::new();
        let mut schedule = Schedule::default();

        schedule.add_systems((
            systems::context_management_system,
            systems::inference_system,
            systems::eviction_system,
            systems::pool_management_system,
            systems::child_completion_system,
            systems::cascade_kill_system,
            systems::stage_gating_system,
        ));

        let (message_tx, message_rx) = mpsc::unbounded_channel();

        Self {
            world,
            schedule,
            provider_registry,
            message_tx,
            message_rx,
        }
    }

    /// Execute one tick of the agent engine.
    ///
    /// This runs all systems in the schedule once, processing all agents.
    pub fn tick(&mut self) {
        self.schedule.run(&mut self.world);
    }

    /// Get a reference to the ECS world.
    pub fn world(&self) -> &World {
        &self.world
    }

    /// Get a mutable reference to the ECS world.
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// Get a reference to the provider registry.
    pub fn providers(&self) -> &ProviderRegistry {
        &self.provider_registry
    }

    /// Get a mutable reference to the provider registry.
    pub fn providers_mut(&mut self) -> &mut ProviderRegistry {
        &mut self.provider_registry
    }

    /// Get a specific provider by name.
    pub fn get_provider(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.provider_registry.get(name).cloned()
    }

    /// Send a message to a running agent via the channel.
    pub fn send_message(&self, msg: AgentMessage) -> std::result::Result<(), ProviderError> {
        self.message_tx
            .send(msg)
            .map_err(|e| ProviderError::Other(format!("Failed to send message: {}", e)))
    }

    /// Get a clone of the message sender for external use.
    pub fn get_message_sender(&self) -> mpsc::UnboundedSender<AgentMessage> {
        self.message_tx.clone()
    }

    /// Process pending messages from the channel, delivering them to agent inboxes.
    pub fn process_messages(&mut self) {
        let mut messages = Vec::new();
        while let Ok(msg) = self.message_rx.try_recv() {
            messages.push(msg);
        }

        for msg in messages {
            // Find the agent entity by ID
            let mut found = false;
            let mut query = self.world.query::<(&AgentState, &mut MessageInbox)>();
            for (state, mut inbox) in query.iter_mut(&mut self.world) {
                if state.agent_id == msg.agent_id {
                    inbox.push(msg.clone());
                    found = true;
                    break;
                }
            }
            if !found {
                tracing::warn!(agent_id = %msg.agent_id, "Message target agent not found");
            }
        }

        // Deliver messages from inboxes to context windows
        self.deliver_inbox_messages();
    }

    /// Deliver messages from agent inboxes into their context windows.
    ///
    /// Only delivers if the agent's current stage accepts messages
    /// (`AgentState.accepts_messages`). If false, messages stay in the inbox
    /// and will be delivered when the stage changes to one that accepts them.
    fn deliver_inbox_messages(&mut self) {
        let mut deliveries: Vec<(Entity, Vec<AgentMessage>)> = Vec::new();

        let mut query = self
            .world
            .query::<(Entity, &AgentState, &mut MessageInbox)>();
        for (entity, state, mut inbox) in query.iter_mut(&mut self.world) {
            if !state.accepts_messages {
                // Leave messages in inbox until a stage that accepts them
                continue;
            }
            let msgs = inbox.drain_all();
            if !msgs.is_empty() {
                deliveries.push((entity, msgs));
            }
        }

        for (entity, msgs) in deliveries {
            if let Some(mut window) = self.world.get_mut::<ContextWindow>(entity) {
                for msg in msgs {
                    let region_name = msg.target_region.as_deref().unwrap_or("conversation");
                    let tokens = msg.content.len() / 4 + 1;
                    let _ =
                        window.add_to_region(region_name, format!("User: {}", msg.content), tokens);
                }
            }
        }
    }

    /// Cancel a running agent by setting its cancellation token and status.
    pub fn cancel_agent(&mut self, agent_id: &str) -> std::result::Result<(), ProviderError> {
        let mut found = false;

        let mut query = self.world.query::<(&mut AgentState, &CancellationToken)>();
        for (mut state, token) in query.iter_mut(&mut self.world) {
            if state.agent_id == agent_id {
                token.cancel();
                state.status = AgentStatus::Cancelled;
                tracing::info!(agent_id = %agent_id, "Agent cancelled");
                found = true;
                break;
            }
        }

        if found {
            Ok(())
        } else {
            Err(ProviderError::Other(format!(
                "Agent '{}' not found",
                agent_id
            )))
        }
    }

    /// Run inference for a specific agent.
    ///
    /// This is the core inference loop:
    /// 1. Build InferenceRequest from the agent's ContextWindow
    /// 2. Look up the correct provider based on model config
    /// 3. Call provider.infer()
    /// 4. Parse tool calls from the response
    /// 5. Add response to conversation region
    /// 6. Return the response for the caller to handle tool execution
    ///
    /// If `tool_filter` is provided, only tools whose names appear in the
    /// filter list will be included in the request.
    pub async fn run_inference(
        &mut self,
        entity: Entity,
        provider_name: &str,
        model: &str,
        tools: Vec<Tool>,
    ) -> std::result::Result<InferenceResponse, ProviderError> {
        self.run_inference_filtered(entity, provider_name, model, tools, None)
            .await
    }

    /// Run inference with an optional tool name filter.
    pub async fn run_inference_filtered(
        &mut self,
        entity: Entity,
        provider_name: &str,
        model: &str,
        tools: Vec<Tool>,
        tool_filter: Option<&[String]>,
    ) -> std::result::Result<InferenceResponse, ProviderError> {
        let provider = self
            .provider_registry
            .get(provider_name)
            .ok_or_else(|| {
                ProviderError::Other(format!("Provider '{}' not registered", provider_name))
            })?
            .clone();

        // Build structured messages from the context window
        let (messages, max_tokens) = {
            let window = self
                .world
                .get::<ContextWindow>(entity)
                .ok_or_else(|| ProviderError::Other("Entity has no ContextWindow".to_string()))?;

            let messages = window.assemble_messages();
            let remaining = window.max_tokens.saturating_sub(window.current_tokens);
            let max_tokens = remaining.min(4096); // Cap at 4096 for response
            (messages, max_tokens)
        };

        // Apply tool filter if provided
        let filtered_tools = if let Some(filter) = tool_filter {
            if filter.is_empty() {
                tools // Empty filter = include all
            } else {
                tools
                    .into_iter()
                    .filter(|t| filter.iter().any(|f| f == &t.name))
                    .collect()
            }
        } else {
            tools // None = include all
        };

        // Respect each model's temperature support (e.g. claude-opus-4-8 deprecates it).
        let temperature = if provider.capabilities(model).supports_temperature {
            0.7
        } else {
            0.0
        };
        let request = InferenceRequest {
            messages,
            model: model.to_string(),
            max_tokens,
            temperature,
            tools: filtered_tools,
            extra: serde_json::Value::Null,
        };

        let response = provider.infer(request).await?;

        // Store the inference result on the entity
        let result = InferenceResult {
            response: response.content.clone(),
            tool_calls: response
                .tool_calls
                .iter()
                .map(|tc| crate::components::ToolCall {
                    tool_id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                })
                .collect(),
            tokens_used: response.tokens_used.total_tokens,
            timestamp: chrono::Utc::now().timestamp(),
        };

        self.world.entity_mut(entity).insert(result);

        // Update agent state
        if let Some(mut state) = self.world.get_mut::<AgentState>(entity) {
            state.iteration += 1;
        }

        Ok(response)
    }

    /// Run the full inference loop for an agent until completion.
    ///
    /// This executes the inference loop:
    /// 1. Call the LLM
    /// 2. If the LLM returns tool calls, execute them (via callback)
    /// 3. Add tool results to context
    /// 4. Repeat until no tool calls or max iterations reached
    ///
    /// Checks the agent's CancellationToken between iterations and returns
    /// early if cancelled.
    pub async fn run_inference_loop<F, Fut>(
        &mut self,
        entity: Entity,
        provider_name: &str,
        model: &str,
        tools: Vec<Tool>,
        max_iterations: usize,
        mut tool_executor: F,
    ) -> std::result::Result<InferenceResponse, ProviderError>
    where
        F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut,
        Fut: std::future::Future<
            Output = Vec<(String, String)>, // Vec<(tool_call_id, result)>
        >,
    {
        self.run_inference_loop_filtered(
            entity,
            provider_name,
            model,
            tools,
            max_iterations,
            None,
            None,
            None,
            &mut tool_executor,
        )
        .await
    }

    /// Run the full inference loop with optional tool filtering and tool result routing.
    ///
    /// `tool_filter`: if Some, only tools matching these names are included.
    /// `tool_result_routing`: if Some, routes tool results to configured regions.
    /// `compaction_config`: if Some, automatically runs eviction + compaction after
    /// each iteration when the context window is filling up.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_inference_loop_filtered<F, Fut>(
        &mut self,
        entity: Entity,
        provider_name: &str,
        model: &str,
        tools: Vec<Tool>,
        max_iterations: usize,
        tool_filter: Option<&[String]>,
        tool_result_routing: Option<&ToolResultRoutingConfig>,
        compaction_config: Option<&leviath_core::CompactionConfig>,
        tool_executor: &mut F,
    ) -> std::result::Result<InferenceResponse, ProviderError>
    where
        F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut,
        Fut: std::future::Future<
            Output = Vec<(String, String)>, // Vec<(tool_call_id, result)>
        >,
    {
        let mut last_response = None;

        for iteration in 0..max_iterations {
            // Check cancellation token before each iteration
            if let Some(token) = self.world.get::<CancellationToken>(entity) {
                if token.is_cancelled() {
                    tracing::info!(iteration, "Inference loop cancelled");
                    if let Some(mut state) = self.world.get_mut::<AgentState>(entity) {
                        state.status = AgentStatus::Cancelled;
                    }
                    return last_response.ok_or_else(|| {
                        ProviderError::Other(
                            "Agent cancelled before producing a response".to_string(),
                        )
                    });
                }
            }

            // Process any pending messages before inference
            self.process_messages();

            tracing::debug!(iteration, "Inference loop iteration");

            let response = self
                .run_inference_filtered(entity, provider_name, model, tools.clone(), tool_filter)
                .await?;

            // Check if we're done (no tool calls)
            if response.tool_calls.is_empty() {
                tracing::info!(
                    iteration,
                    finish_reason = ?response.finish_reason,
                    "Inference loop complete"
                );
                return Ok(response);
            }

            // Execute tool calls
            let tool_calls_snapshot = response.tool_calls.clone();
            let tool_results = tool_executor(tool_calls_snapshot.clone()).await;

            // Add tool results to context window
            if let Some(mut window) = self.world.get_mut::<ContextWindow>(entity) {
                // Add assistant response
                let response_tokens = response.content.len() / 4;
                let _ = window.add_to_region(
                    "conversation",
                    format!("Assistant: {}", response.content),
                    response_tokens,
                );

                // Route tool results based on routing config
                for (tool_call_id, result) in &tool_results {
                    let mut result_text = result.clone();

                    // Apply max_result_tokens truncation if configured
                    if let Some(routing) = tool_result_routing {
                        if let Some(max_tokens) = routing.max_result_tokens {
                            let max_chars = max_tokens * 4; // approximate
                            if result_text.len() > max_chars {
                                result_text.truncate(max_chars);
                                result_text.push_str("\n[...truncated]");
                            }
                        }
                    }

                    let result_tokens = result_text.len() / 4 + 1;
                    let formatted = format!("[Tool {}]: {}", tool_call_id, result_text);

                    // Find the tool name for this tool_call_id to use for routing lookup
                    let tool_name = tool_calls_snapshot
                        .iter()
                        .find(|tc| tc.id == *tool_call_id)
                        .map(|tc| tc.name.as_str())
                        .unwrap_or("");

                    // Determine target region
                    let target_region = if let Some(routing) = tool_result_routing {
                        // Check per-tool overrides using tool NAME (not call ID)
                        if let Some(override_region) = routing.tool_overrides.get(tool_name) {
                            override_region.as_str()
                        } else {
                            routing.default_region.as_str()
                        }
                    } else {
                        "tool_results"
                    };

                    // If persist is false in routing, add to a clearable region instead
                    let actual_region = if let Some(routing) = tool_result_routing {
                        if !routing.persist {
                            // Try clearable scratch region, fall back to target
                            if window.get_region("scratch").is_some() {
                                "scratch"
                            } else {
                                target_region
                            }
                        } else {
                            target_region
                        }
                    } else {
                        target_region
                    };

                    let _ = window.add_to_region(actual_region, formatted, result_tokens);
                }
            }

            // Check for any user messages that arrived during tool execution
            self.process_messages();

            // After adding tool results, check if context needs eviction + compaction
            if let Some(cc) = compaction_config {
                match self.evict_and_compact(entity, cc).await {
                    Ok(freed) if freed > 0 => {
                        tracing::info!(
                            iteration,
                            tokens_freed = freed,
                            "Auto-eviction during inference loop"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(iteration, error = %e, "Auto-eviction/compaction failed during inference loop");
                    }
                    _ => {}
                }
            }

            last_response = Some(response);
        }

        tracing::warn!(max_iterations, "Inference loop hit max iterations");
        last_response.ok_or_else(|| ProviderError::Other("No response generated".to_string()))
    }

    /// Check if the context window needs eviction, evict what can be evicted
    /// synchronously, and then compact any regions that need LLM-based compaction.
    ///
    /// Returns the number of tokens freed (by eviction only; compaction clears
    /// regions separately).
    pub async fn evict_and_compact(
        &mut self,
        entity: Entity,
        compaction_config: &leviath_core::CompactionConfig,
    ) -> std::result::Result<usize, ProviderError> {
        let eviction_result = {
            let mut window = self
                .world
                .get_mut::<ContextWindow>(entity)
                .ok_or_else(|| ProviderError::Other("No ContextWindow".to_string()))?;

            if !window.needs_eviction(0.9) {
                return Ok(0);
            }

            let target_free = window.max_tokens / 10;
            window
                .try_evict(target_free)
                .map_err(|e| ProviderError::Other(e.to_string()))?
        };

        // Compact any regions that need it
        for region_name in &eviction_result.needs_compaction {
            self.compact_region(entity, region_name, compaction_config)
                .await?;
        }

        // Remove NeedsCompaction marker if present (engine handled it)
        self.world.entity_mut(entity).remove::<NeedsCompaction>();

        Ok(eviction_result.tokens_freed)
    }

    /// Perform LLM-based compaction for a specific region.
    ///
    /// Sends the region's content to an LLM for summarization, stores the
    /// summary in the paired CompactHistory region, and clears the original.
    pub async fn compact_region(
        &mut self,
        entity: Entity,
        region_name: &str,
        compaction_config: &leviath_core::CompactionConfig,
    ) -> std::result::Result<(), ProviderError> {
        let provider = self
            .provider_registry
            .get(&compaction_config.provider)
            .ok_or_else(|| {
                ProviderError::Other(format!(
                    "Compaction provider '{}' not registered",
                    compaction_config.provider
                ))
            })?
            .clone();

        // Get region content
        let (content, source_region_name) = {
            let window = self
                .world
                .get::<ContextWindow>(entity)
                .ok_or_else(|| ProviderError::Other("Entity has no ContextWindow".to_string()))?;

            let region = window.get_region(region_name).ok_or_else(|| {
                ProviderError::Other(format!("Region '{}' not found", region_name))
            })?;

            let content: String = region
                .content
                .iter()
                .map(|e| e.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");

            (content, region.name.clone())
        };

        if content.is_empty() {
            return Ok(());
        }

        // Build compaction request
        let system_prompt = compaction_config.system_prompt().to_string();
        let user_prompt = compaction_config.user_prompt(&content, &source_region_name);

        let request = InferenceRequest {
            messages: vec![
                Message {
                    role: "system".to_string(),
                    content: system_prompt,
                    cache_breakpoint: false,
                },
                Message {
                    role: "user".to_string(),
                    content: user_prompt,
                    cache_breakpoint: false,
                },
            ],
            model: compaction_config.model.clone(),
            max_tokens: compaction_config.max_summary_tokens,
            temperature: compaction_config.temperature,
            tools: Vec::new(),
            extra: serde_json::Value::Null,
        };

        let response = provider.infer(request).await?;
        let summary = response.content;
        let summary_tokens = summary.len() / 4; // Approximate

        // Find the paired CompactHistory region and store summary
        if let Some(mut window) = self.world.get_mut::<ContextWindow>(entity) {
            // Find CompactHistory region paired with this source
            let history_region_name = window
                .regions
                .iter()
                .find(|r| {
                    matches!(&r.kind, leviath_core::RegionKind::CompactHistory { source_region }
                        if source_region == region_name)
                })
                .map(|r| r.name.clone());

            if let Some(history_name) = history_region_name {
                let _ = window.add_to_region(&history_name, summary, summary_tokens);
            }

            // Clear the compacting region
            if let Some(region) = window.get_region_mut(region_name) {
                region.clear();
            }

            window.current_tokens = window.calculate_tokens();
        }

        tracing::info!(region = region_name, "Compaction complete");

        Ok(())
    }
}

/// Configuration for routing tool results to specific context window regions.
#[derive(Debug, Clone)]
pub struct ToolResultRoutingConfig {
    /// Default region for tool results (default: "tool_results")
    pub default_region: String,
    /// Per-tool overrides: tool_name → region_name
    pub tool_overrides: HashMap<String, String>,
    /// Whether to keep tool results (true) or discard after use (false)
    pub persist: bool,
    /// Max tokens per tool result (truncate if larger)
    pub max_result_tokens: Option<usize>,
}

impl Default for ToolResultRoutingConfig {
    fn default() -> Self {
        Self {
            default_region: "tool_results".to_string(),
            tool_overrides: HashMap::new(),
            persist: true,
            max_result_tokens: None,
        }
    }
}

impl Default for AgentEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No `tracing::Subscriber` is registered during unit tests, so
    /// multi-line `tracing::info!`/`warn!` calls' field-expression lines
    /// show as 0-hit in `cargo llvm-cov` even when the surrounding branch
    /// runs (the macro's internal "is this level enabled" check
    /// short-circuits before evaluating the fields). Running a test under
    /// this no-op subscriber makes the check pass so the fields actually
    /// execute. Mirrors the identical harness in `systems.rs`.
    struct AlwaysOnSubscriber;

    impl tracing::Subscriber for AlwaysOnSubscriber {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, _event: &tracing::Event<'_>) {}
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    fn with_tracing<T>(f: impl FnOnce() -> T) -> T {
        tracing::subscriber::with_default(AlwaysOnSubscriber, f)
    }

    /// Async-safe variant of `with_tracing`: `tracing::subscriber::with_default`
    /// only wraps a synchronous closure, so calling it around an unawaited
    /// `async fn` call only covers the (instant, side-effect-free) future
    /// *construction*, not the tracing calls that execute later when the
    /// future is polled/awaited. `set_default` instead installs a guard that
    /// stays active for its lifetime, correctly covering every `.await`
    /// point -- valid here because `#[tokio::test]` defaults to a
    /// single-threaded (current-thread) runtime, so the task never hops
    /// threads mid-poll.
    async fn with_tracing_async<T>(f: impl std::future::Future<Output = T>) -> T {
        let _guard = tracing::subscriber::set_default(AlwaysOnSubscriber);
        f.await
    }

    #[test]
    fn always_on_subscriber_span_methods_are_all_no_ops() {
        // This file's tracing calls are all events, never spans -- exercise
        // the subscriber's own otherwise-dead span methods directly via a
        // real span (entered twice, to also hit `record_follows_from`).
        with_tracing(|| {
            let span_a = tracing::info_span!("a", value = tracing::field::Empty);
            span_a.record("value", 1);
            let span_b = tracing::info_span!("b");
            span_b.follows_from(&span_a);
            let _enter_a = span_a.enter();
            let _enter_b = span_b.enter();
        });
    }

    #[test]
    fn test_engine_creation() {
        let engine = AgentEngine::new();
        assert!(engine.world().entities().is_empty());
    }

    #[test]
    fn test_engine_with_providers() {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "ollama".to_string(),
            Arc::new(leviath_providers::OllamaProvider::new()),
        );
        let engine = AgentEngine::with_providers(registry);
        assert!(engine.providers().has("ollama"));
    }

    #[test]
    fn test_provider_registry() {
        let mut registry = ProviderRegistry::new();
        assert!(!registry.has("anthropic"));

        registry.register(
            "anthropic".to_string(),
            Arc::new(leviath_providers::AnthropicProvider::new(
                "test-key".to_string(),
            )),
        );
        assert!(registry.has("anthropic"));
        assert!(registry.get("anthropic").is_some());
        assert_eq!(registry.provider_names().len(), 1);
    }

    #[test]
    fn test_engine_tick() {
        let mut engine = AgentEngine::new();
        // Should not panic with no entities
        engine.tick();
    }

    #[test]
    fn test_message_sender() {
        let engine = AgentEngine::new();
        let sender = engine.get_message_sender();
        // Should be able to send a message (will be queued)
        let msg = crate::components::AgentMessage {
            agent_id: "test-1".to_string(),
            content: "hello".to_string(),
            target_region: None,
            priority: 0,
        };
        assert!(sender.send(msg).is_ok());
    }

    #[test]
    fn test_process_messages_no_agents() {
        let mut engine = AgentEngine::new();
        // Send a message
        let msg = crate::components::AgentMessage {
            agent_id: "nonexistent".to_string(),
            content: "hello".to_string(),
            target_region: None,
            priority: 0,
        };
        engine.send_message(msg).unwrap();
        // Should not panic even with no matching agents
        engine.process_messages();
    }

    #[test]
    fn test_cancel_nonexistent_agent() {
        let mut engine = AgentEngine::new();
        let result = engine.cancel_agent("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn test_tool_result_routing_config_default() {
        let config = ToolResultRoutingConfig::default();
        assert_eq!(config.default_region, "tool_results");
        assert!(config.persist);
        assert!(config.max_result_tokens.is_none());
        assert!(config.tool_overrides.is_empty());
    }

    #[test]
    fn test_deliver_inbox_messages_respects_accepts_messages_false() {
        let mut engine = AgentEngine::new();
        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "test-agent".to_string(),
                    current_stage: "report".to_string(),
                    iteration: 0,
                    status: AgentStatus::Active,
                    spawned_children_ids: Vec::new(),
                    pending_wait: None,
                    accepts_messages: false,
                },
                MessageInbox::new(),
                {
                    let mut window = ContextWindow::new(10000);
                    window.add_region(leviath_core::Region::new(
                        "conversation".to_string(),
                        leviath_core::RegionKind::SlidingWindow { max_items: 50 },
                        8000,
                    ));
                    window
                },
            ))
            .id();

        // Send a message
        engine
            .send_message(AgentMessage {
                agent_id: "test-agent".to_string(),
                content: "hello".to_string(),
                target_region: None,
                priority: 0,
            })
            .unwrap();

        // Process — should route to inbox but NOT deliver to context
        engine.process_messages();

        // Message should still be in inbox
        let inbox = engine.world().get::<MessageInbox>(entity).unwrap();
        #[rustfmt::skip]
        assert_eq!(inbox.messages.len(), 1, "message should stay in inbox when accepts_messages=false");

        // Context should be empty
        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = window.get_region("conversation").unwrap();
        assert!(conv.content.is_empty(), "no messages should be in context");
    }

    #[test]
    fn test_deliver_inbox_messages_delivers_when_accepts_messages_true() {
        let mut engine = AgentEngine::new();
        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "test-agent".to_string(),
                    current_stage: "analyze".to_string(),
                    iteration: 0,
                    status: AgentStatus::Active,
                    spawned_children_ids: Vec::new(),
                    pending_wait: None,
                    accepts_messages: true,
                },
                MessageInbox::new(),
                {
                    let mut window = ContextWindow::new(10000);
                    window.add_region(leviath_core::Region::new(
                        "conversation".to_string(),
                        leviath_core::RegionKind::SlidingWindow { max_items: 50 },
                        8000,
                    ));
                    window
                },
            ))
            .id();

        engine
            .send_message(AgentMessage {
                agent_id: "test-agent".to_string(),
                content: "focus on error handling".to_string(),
                target_region: None,
                priority: 0,
            })
            .unwrap();

        engine.process_messages();

        // Message should be delivered to context
        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = window.get_region("conversation").unwrap();
        assert_eq!(conv.content.len(), 1);
        #[rustfmt::skip]
        assert!(conv.content[0].content.starts_with("User: "), "message should be formatted as 'User: ...' not '[Message]: ...'");
        assert!(conv.content[0].content.contains("focus on error handling"));

        // Inbox should be drained
        let inbox = engine.world().get::<MessageInbox>(entity).unwrap();
        assert!(inbox.messages.is_empty());
    }

    #[test]
    fn test_provider_registry_default() {
        let registry = ProviderRegistry::default();
        assert!(registry.provider_names().is_empty());
    }

    #[test]
    fn test_provider_registry_has_returns_false_for_missing() {
        let registry = ProviderRegistry::new();
        assert!(!registry.has("nonexistent"));
    }

    #[test]
    fn test_provider_registry_get_returns_none_for_missing() {
        let registry = ProviderRegistry::new();
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_provider_registry_multiple_providers() {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "a".to_string(),
            Arc::new(leviath_providers::OllamaProvider::new()),
        );
        registry.register(
            "b".to_string(),
            Arc::new(leviath_providers::OllamaProvider::new()),
        );
        assert_eq!(registry.provider_names().len(), 2);
        assert!(registry.has("a"));
        assert!(registry.has("b"));
    }

    #[test]
    fn test_engine_default() {
        let engine = AgentEngine::default();
        assert!(engine.world().entities().is_empty());
    }

    #[test]
    fn test_engine_providers_accessor() {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "test".to_string(),
            Arc::new(leviath_providers::OllamaProvider::new()),
        );
        let engine = AgentEngine::with_providers(registry);
        assert!(engine.providers().has("test"));
    }

    #[test]
    fn test_engine_providers_mut_accessor() {
        let mut engine = AgentEngine::new();
        assert!(!engine.providers().has("test"));
        engine.providers_mut().register(
            "test".to_string(),
            Arc::new(leviath_providers::OllamaProvider::new()),
        );
        assert!(engine.providers().has("test"));
    }

    #[test]
    fn test_engine_get_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "test".to_string(),
            Arc::new(leviath_providers::OllamaProvider::new()),
        );
        let engine = AgentEngine::with_providers(registry);
        assert!(engine.get_provider("test").is_some());
        assert!(engine.get_provider("nonexistent").is_none());
    }

    #[test]
    fn test_engine_tick_with_entities() {
        let mut engine = AgentEngine::new();
        // Spawn an entity — tick should not panic
        engine.world_mut().spawn(AgentState {
            agent_id: "test".to_string(),
            current_stage: "main".to_string(),
            iteration: 0,
            status: AgentStatus::Active,
            spawned_children_ids: Vec::new(),
            pending_wait: None,
            accepts_messages: true,
        });
        engine.tick();
    }

    #[test]
    fn test_cancel_agent_with_cancellation_token() {
        let mut engine = AgentEngine::new();
        let token = CancellationToken::new();
        engine.world_mut().spawn((
            AgentState {
                agent_id: "cancel-me".to_string(),
                current_stage: "main".to_string(),
                iteration: 0,
                status: AgentStatus::Active,
                spawned_children_ids: Vec::new(),
                pending_wait: None,
                accepts_messages: true,
            },
            token,
        ));

        let result = engine.cancel_agent("cancel-me");
        assert!(result.is_ok());

        // Verify status changed
        let mut query = engine.world.query::<&AgentState>();
        for state in query.iter(&engine.world) {
            if state.agent_id == "cancel-me" {
                assert!(matches!(state.status, AgentStatus::Cancelled));
            }
        }
    }

    #[test]
    fn test_send_message_to_existing_agent() {
        let mut engine = AgentEngine::new();
        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "msg-agent".to_string(),
                    current_stage: "main".to_string(),
                    iteration: 0,
                    status: AgentStatus::Active,
                    spawned_children_ids: Vec::new(),
                    pending_wait: None,
                    accepts_messages: true,
                },
                MessageInbox::new(),
                {
                    let mut window = ContextWindow::new(10000);
                    window.add_region(leviath_core::Region::new(
                        "conversation".to_string(),
                        leviath_core::RegionKind::SlidingWindow { max_items: 50 },
                        8000,
                    ));
                    window
                },
            ))
            .id();

        engine
            .send_message(AgentMessage {
                agent_id: "msg-agent".to_string(),
                content: "message 1".to_string(),
                target_region: None,
                priority: 0,
            })
            .unwrap();

        engine
            .send_message(AgentMessage {
                agent_id: "msg-agent".to_string(),
                content: "message 2".to_string(),
                target_region: None,
                priority: 0,
            })
            .unwrap();

        engine.process_messages();

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = window.get_region("conversation").unwrap();
        assert_eq!(conv.content.len(), 2);
    }

    #[test]
    fn test_message_with_target_region() {
        let mut engine = AgentEngine::new();
        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "region-agent".to_string(),
                    current_stage: "main".to_string(),
                    iteration: 0,
                    status: AgentStatus::Active,
                    spawned_children_ids: Vec::new(),
                    pending_wait: None,
                    accepts_messages: true,
                },
                MessageInbox::new(),
                {
                    let mut window = ContextWindow::new(10000);
                    window.add_region(leviath_core::Region::new(
                        "conversation".to_string(),
                        leviath_core::RegionKind::SlidingWindow { max_items: 50 },
                        8000,
                    ));
                    window.add_region(leviath_core::Region::new(
                        "custom".to_string(),
                        leviath_core::RegionKind::SlidingWindow { max_items: 10 },
                        2000,
                    ));
                    window
                },
            ))
            .id();

        engine
            .send_message(AgentMessage {
                agent_id: "region-agent".to_string(),
                content: "custom message".to_string(),
                target_region: Some("custom".to_string()),
                priority: 0,
            })
            .unwrap();

        engine.process_messages();

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        let custom = window.get_region("custom").unwrap();
        assert_eq!(custom.content.len(), 1);
        // conversation should be empty
        let conv = window.get_region("conversation").unwrap();
        assert!(conv.content.is_empty());
    }

    #[test]
    fn test_tool_result_routing_config_with_overrides() {
        let mut config = ToolResultRoutingConfig::default();
        config
            .tool_overrides
            .insert("bash".to_string(), "scratch".to_string());
        config.max_result_tokens = Some(1000);
        config.persist = false;

        assert_eq!(config.tool_overrides.get("bash").unwrap(), "scratch");
        assert_eq!(config.max_result_tokens, Some(1000));
        assert!(!config.persist);
    }

    #[tokio::test]
    async fn test_run_inference_missing_provider() {
        let mut engine = AgentEngine::new();
        let entity = engine.world_mut().spawn(ContextWindow::new(1000)).id();

        let result = engine
            .run_inference(entity, "nonexistent", "model", Vec::new())
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not registered"));
    }

    #[tokio::test]
    async fn test_run_inference_missing_context_window() {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "test".to_string(),
            Arc::new(leviath_providers::OllamaProvider::new()),
        );
        let mut engine = AgentEngine::with_providers(registry);
        let entity = engine.world_mut().spawn_empty().id();

        let result = engine
            .run_inference(entity, "test", "model", Vec::new())
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("ContextWindow"));
    }

    #[tokio::test]
    async fn test_evict_and_compact_missing_context_window() {
        let mut engine = AgentEngine::new();
        let entity = engine.world_mut().spawn_empty().id();

        let cc = leviath_core::CompactionConfig {
            provider: "test".to_string(),
            model: "test".to_string(),
            max_summary_tokens: 500,
            temperature: 0.3,
            system_prompt: None,
            user_prompt_template: None,
        };

        let result = engine.evict_and_compact(entity, &cc).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_evict_and_compact_below_threshold() {
        let mut engine = AgentEngine::new();
        let entity = engine
            .world_mut()
            .spawn({
                let mut window = ContextWindow::new(100000);
                window.add_region(leviath_core::Region::new(
                    "conversation".to_string(),
                    leviath_core::RegionKind::SlidingWindow { max_items: 50 },
                    80000,
                ));
                window
            })
            .id();

        let cc = leviath_core::CompactionConfig {
            provider: "test".to_string(),
            model: "test".to_string(),
            max_summary_tokens: 500,
            temperature: 0.3,
            system_prompt: None,
            user_prompt_template: None,
        };

        // Window is not at 90% capacity, so no eviction needed
        let result = engine.evict_and_compact(entity, &cc).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[tokio::test]
    async fn test_evict_and_compact_frees_tokens_via_temporary_eviction() {
        // A Temporary region alone is enough to satisfy target_free_tokens,
        // so eviction succeeds with tokens_freed > 0 and no compaction needed.
        let mut engine = AgentEngine::new();
        let entity = engine
            .world_mut()
            .spawn({
                let mut window = ContextWindow::new(1000);
                let mut temp = leviath_core::Region::new(
                    "scratch".to_string(),
                    leviath_core::RegionKind::Temporary,
                    1000,
                );
                temp.add_entry("disposable tool output".to_string(), 950)
                    .unwrap();
                window.add_region(temp);
                window.current_tokens = 950; // 95% full -> needs_eviction(0.9) true
                window
            })
            .id();

        let cc = leviath_core::CompactionConfig {
            provider: "unused".to_string(),
            model: "test".to_string(),
            max_summary_tokens: 500,
            temperature: 0.3,
            system_prompt: None,
            user_prompt_template: None,
        };

        let result = engine.evict_and_compact(entity, &cc).await;
        assert!(result.is_ok());
        assert!(result.unwrap() > 0, "expected tokens to be freed");
    }

    #[tokio::test]
    async fn test_evict_and_compact_runs_compaction_for_needed_regions() {
        // Only a Compacting region over threshold, nothing Temporary/Clearable
        // to evict -> try_evict reports needs_compaction, and evict_and_compact
        // must call compact_region for it.
        let mut registry = ProviderRegistry::new();
        registry.register(
            "compact-provider".to_string(),
            Arc::new(MockProvider::new("compact-provider")),
        );
        let mut engine = AgentEngine::with_providers(registry);

        let entity = engine
            .world_mut()
            .spawn({
                let mut window = ContextWindow::new(700);
                let mut compacting = leviath_core::Region::new(
                    "analysis".to_string(),
                    leviath_core::RegionKind::Compacting {
                        threshold_tokens: 500,
                    },
                    700,
                );
                compacting
                    .add_entry("lots of analysis".to_string(), 650)
                    .unwrap();
                window.add_region(compacting);
                window.current_tokens = 650; // 650/700 ~= 0.93 -> needs_eviction(0.9) true
                window
            })
            .id();

        let cc = leviath_core::CompactionConfig {
            provider: "compact-provider".to_string(),
            model: "test".to_string(),
            max_summary_tokens: 200,
            temperature: 0.3,
            system_prompt: None,
            user_prompt_template: None,
        };

        let result = engine.evict_and_compact(entity, &cc).await;
        #[rustfmt::skip]
        assert!(result.is_ok(), "expected compaction to succeed: {:?}", result.err());

        // The compacting region should have been cleared by compact_region.
        let w = engine.world().get::<ContextWindow>(entity).unwrap();
        let region = w.get_region("analysis").unwrap();
        assert!(region.content.is_empty());
    }

    #[tokio::test]
    async fn test_evict_and_compact_propagates_pinned_over_budget_error() {
        // Pinned regions alone exceed max_tokens -> try_evict returns
        // PinnedRegionsOverBudget, which evict_and_compact must propagate.
        let mut engine = AgentEngine::new();
        let entity = engine
            .world_mut()
            .spawn({
                let mut window = ContextWindow::new(1000);
                let mut pinned = leviath_core::Region::new(
                    "architecture".to_string(),
                    leviath_core::RegionKind::Pinned,
                    2000,
                );
                pinned
                    .add_entry("huge pinned doc".to_string(), 1500)
                    .unwrap();
                window.add_region(pinned);
                window.current_tokens = 1500; // over max_tokens -> needs_eviction(0.9) true
                window
            })
            .id();

        let cc = leviath_core::CompactionConfig {
            provider: "unused".to_string(),
            model: "test".to_string(),
            max_summary_tokens: 500,
            temperature: 0.3,
            system_prompt: None,
            user_prompt_template: None,
        };

        let result = engine.evict_and_compact(entity, &cc).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.to_lowercase().contains("pinned"),
            "expected a pinned-regions-over-budget error, got: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_run_inference_loop_filtered_auto_eviction_frees_tokens() {
        // First response has a tool call (so the loop reaches the
        // post-tool-execution auto-eviction check); by then the window is
        // over the 90% threshold via a Temporary region, so evict_and_compact
        // returns Ok(freed) with freed > 0 -- the "Auto-eviction during
        // inference loop" info-log branch.
        let responses = vec![
            InferenceResponse {
                content: "using a tool".to_string(),
                tool_calls: vec![leviath_providers::ToolCall {
                    id: "call_1".to_string(),
                    name: "noop".to_string(),
                    arguments: serde_json::json!({}),
                }],
                tokens_used: leviath_providers::TokenUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                finish_reason: leviath_providers::FinishReason::ToolCall,
            },
            default_response(),
        ];
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::with_responses("mock", responses)),
        );
        let mut engine = AgentEngine::with_providers(registry);

        let entity = engine
            .world_mut()
            .spawn({
                let mut window = ContextWindow::new(1000);
                let mut temp = leviath_core::Region::new(
                    "scratch".to_string(),
                    leviath_core::RegionKind::Temporary,
                    1000,
                );
                temp.add_entry("disposable".to_string(), 920).unwrap();
                window.add_region(temp);
                window.add_region(leviath_core::Region::new(
                    "tool_results".to_string(),
                    leviath_core::RegionKind::SlidingWindow { max_items: 50 },
                    50,
                ));
                window.current_tokens = 920;
                window
            })
            .id();

        let cc = leviath_core::CompactionConfig {
            provider: "unused".to_string(),
            model: "test".to_string(),
            max_summary_tokens: 200,
            temperature: 0.3,
            system_prompt: None,
            user_prompt_template: None,
        };

        // Wrapped in `with_tracing_async` so the "Auto-eviction during
        // inference loop" tracing::info! call's field expressions actually
        // execute.
        let result = with_tracing_async(engine.run_inference_loop_filtered(
            entity,
            "mock",
            "test-model",
            Vec::new(),
            5,
            None,
            None,
            Some(&cc),
            &mut |_tool_calls| async { vec![("call_1".to_string(), "ok".to_string())] },
        ))
        .await;
        #[rustfmt::skip]
        assert!(result.is_ok(), "expected loop to complete: {:?}", result.err());
    }

    #[tokio::test]
    async fn test_run_inference_loop_filtered_auto_eviction_error_logs_warning_and_continues() {
        // Same shape as above, but the window's only occupied region is
        // Pinned and over budget -> evict_and_compact returns Err, which
        // must be logged and swallowed (loop continues to iteration 2,
        // which completes normally), not propagated as a loop failure.
        let responses = vec![
            InferenceResponse {
                content: "using a tool".to_string(),
                tool_calls: vec![leviath_providers::ToolCall {
                    id: "call_1".to_string(),
                    name: "noop".to_string(),
                    arguments: serde_json::json!({}),
                }],
                tokens_used: leviath_providers::TokenUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                finish_reason: leviath_providers::FinishReason::ToolCall,
            },
            default_response(),
        ];
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::with_responses("mock", responses)),
        );
        let mut engine = AgentEngine::with_providers(registry);

        let entity = engine
            .world_mut()
            .spawn({
                let mut window = ContextWindow::new(1000);
                let mut pinned = leviath_core::Region::new(
                    "architecture".to_string(),
                    leviath_core::RegionKind::Pinned,
                    2000,
                );
                pinned
                    .add_entry("huge pinned doc".to_string(), 1500)
                    .unwrap();
                window.add_region(pinned);
                window.add_region(leviath_core::Region::new(
                    "tool_results".to_string(),
                    leviath_core::RegionKind::SlidingWindow { max_items: 50 },
                    50,
                ));
                window.current_tokens = 1500;
                window
            })
            .id();

        let cc = leviath_core::CompactionConfig {
            provider: "unused".to_string(),
            model: "test".to_string(),
            max_summary_tokens: 200,
            temperature: 0.3,
            system_prompt: None,
            user_prompt_template: None,
        };

        let result = engine
            .run_inference_loop_filtered(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                5,
                None,
                None,
                Some(&cc),
                &mut |_tool_calls| async { vec![("call_1".to_string(), "ok".to_string())] },
            )
            .await;
        assert!(
            result.is_ok(),
            "auto-eviction errors must be logged, not propagated: {:?}",
            result.err()
        );
    }

    #[tokio::test]
    async fn test_run_inference_loop_filtered_auto_eviction_noop_below_threshold() {
        // compaction_config is Some, but the window is well below the 90%
        // threshold -> evict_and_compact's early-return Ok(0) hits the `_`
        // (no-op) match arm in the loop, neither logging nor erroring.
        let responses = vec![
            InferenceResponse {
                content: "using a tool".to_string(),
                tool_calls: vec![leviath_providers::ToolCall {
                    id: "call_1".to_string(),
                    name: "noop".to_string(),
                    arguments: serde_json::json!({}),
                }],
                tokens_used: leviath_providers::TokenUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                finish_reason: leviath_providers::FinishReason::ToolCall,
            },
            default_response(),
        ];
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::with_responses("mock", responses)),
        );
        let mut engine = AgentEngine::with_providers(registry);

        let entity = engine
            .world_mut()
            .spawn({
                let mut window = ContextWindow::new(10000);
                window.add_region(leviath_core::Region::new(
                    "conversation".to_string(),
                    leviath_core::RegionKind::SlidingWindow { max_items: 50 },
                    8000,
                ));
                window.add_region(leviath_core::Region::new(
                    "tool_results".to_string(),
                    leviath_core::RegionKind::SlidingWindow { max_items: 50 },
                    2000,
                ));
                window
            })
            .id();

        let cc = leviath_core::CompactionConfig {
            provider: "unused".to_string(),
            model: "test".to_string(),
            max_summary_tokens: 200,
            temperature: 0.3,
            system_prompt: None,
            user_prompt_template: None,
        };

        let result = engine
            .run_inference_loop_filtered(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                5,
                None,
                None,
                Some(&cc),
                &mut |_tool_calls| async { vec![("call_1".to_string(), "ok".to_string())] },
            )
            .await;
        assert!(result.is_ok());
    }

    // ─── MockProvider for inference tests ─────────────────────────────────

    struct MockProvider {
        name: String,
        /// If non-empty, each call pops from the front.
        responses: std::sync::Mutex<Vec<InferenceResponse>>,
    }

    impl MockProvider {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                responses: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn with_responses(name: &str, responses: Vec<InferenceResponse>) -> Self {
            Self {
                name: name.to_string(),
                responses: std::sync::Mutex::new(responses),
            }
        }
    }

    fn default_response() -> InferenceResponse {
        InferenceResponse {
            content: "mock response".to_string(),
            tool_calls: vec![],
            tokens_used: leviath_providers::TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            finish_reason: leviath_providers::FinishReason::Complete,
        }
    }

    #[async_trait::async_trait]
    impl leviath_providers::Provider for MockProvider {
        async fn infer(
            &self,
            _req: InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            let mut resps = self.responses.lock().unwrap();
            if resps.is_empty() {
                Ok(default_response())
            } else {
                Ok(resps.remove(0))
            }
        }

        fn count_tokens(&self, _text: &str, _model: &str) -> usize {
            4
        }
        fn max_context_tokens(&self, _model: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            &self.name
        }
        fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    fn make_engine_with_mock() -> (AgentEngine, Entity) {
        let mut registry = ProviderRegistry::new();
        registry.register("mock".to_string(), Arc::new(MockProvider::new("mock")));
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "system".to_string(),
            leviath_core::RegionKind::Pinned,
            2000,
        ));
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            6000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            2000,
        ));
        // Add a system message so assemble_messages returns something
        let _ = window.add_to_region("system", "You are a helpful assistant.".to_string(), 6);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "test-mock".to_string(),
                    current_stage: "main".to_string(),
                    iteration: 0,
                    status: AgentStatus::Active,
                    spawned_children_ids: Vec::new(),
                    pending_wait: None,
                    accepts_messages: true,
                },
                MessageInbox::new(),
                CancellationToken::new(),
                window,
            ))
            .id();

        (engine, entity)
    }

    #[tokio::test]
    async fn test_run_inference_with_mock_provider() {
        let (mut engine, entity) = make_engine_with_mock();

        let result = engine
            .run_inference(entity, "mock", "test-model", Vec::new())
            .await;
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp.content, "mock response");
        assert!(resp.tool_calls.is_empty());
        assert_eq!(resp.tokens_used.total_tokens, 15);

        // Check that iteration was incremented
        let state = engine.world().get::<AgentState>(entity).unwrap();
        assert_eq!(state.iteration, 1);

        // Check that InferenceResult was stored
        let ir = engine.world().get::<InferenceResult>(entity).unwrap();
        assert_eq!(ir.response, "mock response");
        assert_eq!(ir.tokens_used, 15);
    }

    /// A provider whose model doesn't support temperature sampling (e.g. some
    /// reasoning models deprecate it) — used to exercise the `else` branch of
    /// `run_inference_filtered`'s temperature selection, which every other
    /// test (via `MockProvider`, capabilities always default-true) never hits.
    struct NoTemperatureMockProvider;

    #[async_trait::async_trait]
    impl leviath_providers::Provider for NoTemperatureMockProvider {
        async fn infer(
            &self,
            request: InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            // Surface the temperature the engine actually chose, so the test
            // can assert on it without needing internal access.
            Ok(InferenceResponse {
                content: format!("temperature={}", request.temperature),
                tool_calls: vec![],
                tokens_used: leviath_providers::TokenUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                finish_reason: leviath_providers::FinishReason::Complete,
            })
        }
        fn count_tokens(&self, _text: &str, _model: &str) -> usize {
            4
        }
        fn max_context_tokens(&self, _model: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "no-temp-mock"
        }
        fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities {
                supports_temperature: false,
                ..Default::default()
            }
        }
    }

    #[tokio::test]
    async fn test_run_inference_omits_temperature_when_unsupported() {
        let (mut engine, entity) = make_engine_with_mock();
        engine
            .providers_mut()
            .register("no-temp".to_string(), Arc::new(NoTemperatureMockProvider));

        let result = engine
            .run_inference(entity, "no-temp", "reasoning-model", Vec::new())
            .await
            .unwrap();
        assert_eq!(result.content, "temperature=0");
    }

    #[tokio::test]
    async fn test_run_inference_filtered_empty_filter_includes_all() {
        let (mut engine, entity) = make_engine_with_mock();

        let tools = vec![leviath_providers::Tool {
            name: "bash".to_string(),
            description: "Run command".to_string(),
            parameters: serde_json::json!({}),
        }];

        // Empty filter should include all tools
        let result = engine
            .run_inference_filtered(entity, "mock", "test-model", tools, Some(&[]))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_run_inference_filtered_with_matching_filter() {
        let (mut engine, entity) = make_engine_with_mock();

        let tools = vec![
            leviath_providers::Tool {
                name: "bash".to_string(),
                description: "Run command".to_string(),
                parameters: serde_json::json!({}),
            },
            leviath_providers::Tool {
                name: "read_file".to_string(),
                description: "Read file".to_string(),
                parameters: serde_json::json!({}),
            },
        ];

        // Filter to only include "bash"
        let filter = vec!["bash".to_string()];
        let result = engine
            .run_inference_filtered(entity, "mock", "test-model", tools, Some(&filter))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_run_inference_filtered_none_filter_includes_all() {
        let (mut engine, entity) = make_engine_with_mock();

        let tools = vec![leviath_providers::Tool {
            name: "bash".to_string(),
            description: "Run command".to_string(),
            parameters: serde_json::json!({}),
        }];

        // None filter should include all tools
        let result = engine
            .run_inference_filtered(entity, "mock", "test-model", tools, None)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_run_inference_loop_no_tool_calls() {
        let (mut engine, entity) = make_engine_with_mock();

        // Default mock returns no tool calls, so loop should return after first
        // iteration -- wrapped in `with_tracing_async` so the "Inference loop
        // complete" tracing::info! call's field expressions actually execute.
        let result = with_tracing_async(engine.run_inference_loop(
            entity,
            "mock",
            "test-model",
            Vec::new(),
            10,
            |_tool_calls| async { vec![] },
        ))
        .await;
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp.content, "mock response");
    }

    #[tokio::test]
    async fn test_run_inference_loop_with_cancellation() {
        let (mut engine, entity) = make_engine_with_mock();

        // Cancel the token before running
        {
            let token = engine.world().get::<CancellationToken>(entity).unwrap();
            token.cancel();
        }

        let result = engine
            .run_inference_loop(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                10,
                |_tool_calls| async { vec![] },
            )
            .await;
        // Should return error since cancelled before any response
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("cancelled"));

        // Status should be Cancelled
        let state = engine.world().get::<AgentState>(entity).unwrap();
        assert!(matches!(state.status, AgentStatus::Cancelled));
    }

    #[tokio::test]
    async fn test_run_inference_loop_with_tool_calls_then_completion() {
        // First response has tool calls, second has none (completion)
        let responses = vec![
            InferenceResponse {
                content: "let me run a tool".to_string(),
                tool_calls: vec![leviath_providers::ToolCall {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::json!({"cmd": "ls"}),
                }],
                tokens_used: leviath_providers::TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                finish_reason: leviath_providers::FinishReason::ToolCall,
            },
            default_response(), // No tool calls = completion
        ];

        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::with_responses("mock", responses)),
        );
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            6000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            2000,
        ));
        let _ = window.add_to_region("conversation", "User: hi".to_string(), 2);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "tool-test".to_string(),
                    current_stage: "main".to_string(),
                    iteration: 0,
                    status: AgentStatus::Active,
                    spawned_children_ids: Vec::new(),
                    pending_wait: None,
                    accepts_messages: true,
                },
                MessageInbox::new(),
                CancellationToken::new(),
                window,
            ))
            .id();

        let result = engine
            .run_inference_loop(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                10,
                |_tool_calls| async { vec![("call_1".to_string(), "file1\nfile2".to_string())] },
            )
            .await;

        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp.content, "mock response");

        // iteration should be 2 (two inferences)
        let state = engine.world().get::<AgentState>(entity).unwrap();
        assert_eq!(state.iteration, 2);
    }

    #[tokio::test]
    async fn test_run_inference_loop_filtered_with_routing_and_truncation() {
        let responses = vec![
            InferenceResponse {
                content: "calling tool".to_string(),
                tool_calls: vec![leviath_providers::ToolCall {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::json!({"cmd": "ls"}),
                }],
                tokens_used: leviath_providers::TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                finish_reason: leviath_providers::FinishReason::ToolCall,
            },
            default_response(),
        ];

        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::with_responses("mock", responses)),
        );
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            6000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            2000,
        ));
        let _ = window.add_to_region("conversation", "User: hi".to_string(), 2);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "routing-test".to_string(),
                    current_stage: "main".to_string(),
                    iteration: 0,
                    status: AgentStatus::Active,
                    spawned_children_ids: Vec::new(),
                    pending_wait: None,
                    accepts_messages: true,
                },
                MessageInbox::new(),
                CancellationToken::new(),
                window,
            ))
            .id();

        let routing = ToolResultRoutingConfig {
            default_region: "tool_results".to_string(),
            tool_overrides: HashMap::new(),
            persist: true,
            max_result_tokens: Some(2), // very small = truncation at 8 chars
        };

        let result = engine
            .run_inference_loop_filtered(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                10,
                None,
                Some(&routing),
                None,
                &mut |_tool_calls| async {
                    vec![(
                        "call_1".to_string(),
                        "this is a very long tool result that should be truncated".to_string(),
                    )]
                },
            )
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_run_inference_loop_max_iterations() {
        // Every response has tool calls, so loop should hit max_iterations
        let tool_response = InferenceResponse {
            content: "running tools".to_string(),
            tool_calls: vec![leviath_providers::ToolCall {
                id: "call_1".to_string(),
                name: "bash".to_string(),
                arguments: serde_json::json!({}),
            }],
            tokens_used: leviath_providers::TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            finish_reason: leviath_providers::FinishReason::ToolCall,
        };

        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::with_responses(
                "mock",
                vec![tool_response.clone(), tool_response.clone(), tool_response],
            )),
        );
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(100000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            60000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            20000,
        ));
        let _ = window.add_to_region("conversation", "User: hi".to_string(), 2);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "max-iter-test".to_string(),
                    current_stage: "main".to_string(),
                    iteration: 0,
                    status: AgentStatus::Active,
                    spawned_children_ids: Vec::new(),
                    pending_wait: None,
                    accepts_messages: true,
                },
                MessageInbox::new(),
                CancellationToken::new(),
                window,
            ))
            .id();

        let result = engine
            .run_inference_loop(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                3, // max 3 iterations
                |_tool_calls| async { vec![("call_1".to_string(), "ok".to_string())] },
            )
            .await;

        // Should return last response (not an error) since we had responses
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_compact_region_with_mock() {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "compact-provider".to_string(),
            Arc::new(MockProvider::new("compact-provider")),
        );
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            6000,
        ));
        window.add_region(leviath_core::Region::new(
            "conversation_history".to_string(),
            leviath_core::RegionKind::CompactHistory {
                source_region: "conversation".to_string(),
            },
            2000,
        ));
        // Add some content to compact
        let _ = window.add_to_region("conversation", "Message 1".to_string(), 5);
        let _ = window.add_to_region("conversation", "Message 2".to_string(), 5);

        let entity = engine.world_mut().spawn(window).id();

        let cc = leviath_core::CompactionConfig {
            provider: "compact-provider".to_string(),
            model: "test-model".to_string(),
            max_summary_tokens: 200,
            temperature: 0.3,
            system_prompt: None,
            user_prompt_template: None,
        };

        let result = engine.compact_region(entity, "conversation", &cc).await;
        assert!(result.is_ok());

        // After compaction, conversation should be cleared
        let w = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = w.get_region("conversation").unwrap();
        #[rustfmt::skip]
        assert!(conv.content.is_empty(), "conversation should be cleared after compaction");

        // History should have the summary
        let hist = w.get_region("conversation_history").unwrap();
        #[rustfmt::skip]
        assert!(!hist.content.is_empty(), "history should contain the summary");
    }

    #[tokio::test]
    async fn test_compact_region_empty_content_is_noop() {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "compact-provider".to_string(),
            Arc::new(MockProvider::new("compact-provider")),
        );
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            6000,
        ));
        // No content added — should be empty

        let entity = engine.world_mut().spawn(window).id();

        let cc = leviath_core::CompactionConfig {
            provider: "compact-provider".to_string(),
            model: "test-model".to_string(),
            max_summary_tokens: 200,
            temperature: 0.3,
            system_prompt: None,
            user_prompt_template: None,
        };

        let result = engine.compact_region(entity, "conversation", &cc).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_compact_region_missing_provider() {
        let mut engine = AgentEngine::new();
        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            6000,
        ));
        let _ = window.add_to_region("conversation", "content".to_string(), 3);
        let entity = engine.world_mut().spawn(window).id();

        let cc = leviath_core::CompactionConfig {
            provider: "nonexistent".to_string(),
            model: "test-model".to_string(),
            max_summary_tokens: 200,
            temperature: 0.3,
            system_prompt: None,
            user_prompt_template: None,
        };

        let result = engine.compact_region(entity, "conversation", &cc).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_compact_region_missing_region() {
        let mut registry = ProviderRegistry::new();
        registry.register("mock".to_string(), Arc::new(MockProvider::new("mock")));
        let mut engine = AgentEngine::with_providers(registry);
        let window = ContextWindow::new(10000);
        let entity = engine.world_mut().spawn(window).id();

        let cc = leviath_core::CompactionConfig {
            provider: "mock".to_string(),
            model: "test-model".to_string(),
            max_summary_tokens: 200,
            temperature: 0.3,
            system_prompt: None,
            user_prompt_template: None,
        };

        let result = engine.compact_region(entity, "nonexistent", &cc).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_run_inference_loop_filtered_non_persist_routing() {
        let responses = vec![
            InferenceResponse {
                content: "calling tool".to_string(),
                tool_calls: vec![leviath_providers::ToolCall {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::json!({}),
                }],
                tokens_used: leviath_providers::TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                finish_reason: leviath_providers::FinishReason::ToolCall,
            },
            default_response(),
        ];

        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::with_responses("mock", responses)),
        );
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            4000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            2000,
        ));
        window.add_region(leviath_core::Region::new(
            "scratch".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            2000,
        ));
        let _ = window.add_to_region("conversation", "User: hi".to_string(), 2);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "persist-test".to_string(),
                    current_stage: "main".to_string(),
                    iteration: 0,
                    status: AgentStatus::Active,
                    spawned_children_ids: Vec::new(),
                    pending_wait: None,
                    accepts_messages: true,
                },
                MessageInbox::new(),
                CancellationToken::new(),
                window,
            ))
            .id();

        // persist=false and scratch region exists, so tool results go to "scratch"
        let routing = ToolResultRoutingConfig {
            default_region: "tool_results".to_string(),
            tool_overrides: HashMap::new(),
            persist: false,
            max_result_tokens: None,
        };

        let result = engine
            .run_inference_loop_filtered(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                10,
                None,
                Some(&routing),
                None,
                &mut |_tool_calls| async {
                    vec![("call_1".to_string(), "tool output".to_string())]
                },
            )
            .await;

        assert!(result.is_ok());

        // Tool results should be in scratch (not tool_results) because persist=false
        let w = engine.world().get::<ContextWindow>(entity).unwrap();
        let scratch = w.get_region("scratch").unwrap();
        #[rustfmt::skip]
        assert!(!scratch.content.is_empty(), "scratch should have tool results when persist=false");
    }

    #[tokio::test]
    async fn test_run_inference_loop_filtered_non_persist_routing_falls_back_without_scratch() {
        // Same as `..._non_persist_routing` above, but the window has no
        // "scratch" region at all -- exercises the `else { target_region }`
        // fallback inside the `!routing.persist` branch, distinct from the
        // "scratch exists" case the other test covers.
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::with_responses(
                "mock",
                vec![
                    InferenceResponse {
                        content: "calling tool".to_string(),
                        tool_calls: vec![leviath_providers::ToolCall {
                            id: "call_1".to_string(),
                            name: "bash".to_string(),
                            arguments: serde_json::json!({}),
                        }],
                        tokens_used: leviath_providers::TokenUsage {
                            prompt_tokens: 1,
                            completion_tokens: 1,
                            total_tokens: 2,
                            cached_tokens: 0,
                            cache_write_tokens: 0,
                        },
                        finish_reason: leviath_providers::FinishReason::ToolCall,
                    },
                    default_response(),
                ],
            )),
        );
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            4000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            2000,
        ));
        // No "scratch" region.

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "persist-test-no-scratch".to_string(),
                    current_stage: "main".to_string(),
                    iteration: 0,
                    status: AgentStatus::Active,
                    spawned_children_ids: Vec::new(),
                    pending_wait: None,
                    accepts_messages: true,
                },
                MessageInbox::new(),
                CancellationToken::new(),
                window,
            ))
            .id();

        let routing = ToolResultRoutingConfig {
            default_region: "tool_results".to_string(),
            tool_overrides: HashMap::new(),
            persist: false,
            max_result_tokens: None,
        };

        let result = engine
            .run_inference_loop_filtered(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                10,
                None,
                Some(&routing),
                None,
                &mut |_tool_calls| async {
                    vec![("call_1".to_string(), "tool output".to_string())]
                },
            )
            .await;
        assert!(result.is_ok());

        // No scratch region exists, so results fall back to the default
        // ("tool_results") target region even though persist=false.
        let w = engine.world().get::<ContextWindow>(entity).unwrap();
        let tool_results = w.get_region("tool_results").unwrap();
        #[rustfmt::skip]
        assert!(!tool_results.content.is_empty(), "tool_results should have results when scratch is absent");
    }

    #[tokio::test]
    async fn test_run_inference_loop_filtered_tool_override_routing() {
        let responses = vec![
            InferenceResponse {
                content: "calling tool".to_string(),
                tool_calls: vec![leviath_providers::ToolCall {
                    id: "call_1".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::json!({}),
                }],
                tokens_used: leviath_providers::TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                finish_reason: leviath_providers::FinishReason::ToolCall,
            },
            default_response(),
        ];

        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::with_responses("mock", responses)),
        );
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            4000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            2000,
        ));
        window.add_region(leviath_core::Region::new(
            "bash_output".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            2000,
        ));
        let _ = window.add_to_region("conversation", "User: hi".to_string(), 2);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "override-test".to_string(),
                    current_stage: "main".to_string(),
                    iteration: 0,
                    status: AgentStatus::Active,
                    spawned_children_ids: Vec::new(),
                    pending_wait: None,
                    accepts_messages: true,
                },
                MessageInbox::new(),
                CancellationToken::new(),
                window,
            ))
            .id();

        let mut overrides = HashMap::new();
        overrides.insert("bash".to_string(), "bash_output".to_string());
        let routing = ToolResultRoutingConfig {
            default_region: "tool_results".to_string(),
            tool_overrides: overrides,
            persist: true,
            max_result_tokens: None,
        };

        let result = engine
            .run_inference_loop_filtered(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                10,
                None,
                Some(&routing),
                None,
                &mut |_tool_calls| async {
                    vec![("call_1".to_string(), "file listing".to_string())]
                },
            )
            .await;

        assert!(result.is_ok());

        // bash tool results should go to bash_output (override)
        let w = engine.world().get::<ContextWindow>(entity).unwrap();
        let bash_output = w.get_region("bash_output").unwrap();
        assert!(
            !bash_output.content.is_empty(),
            "bash tool results should route to override region"
        );
    }

    #[test]
    fn test_mock_providers_trivial_trait_methods() {
        use leviath_providers::Provider;

        let mock = MockProvider::new("trivial-mock");
        assert_eq!(mock.count_tokens("anything", "any-model"), 4);
        assert_eq!(mock.max_context_tokens("any-model"), 100_000);
        assert_eq!(mock.name(), "trivial-mock");

        let no_temp = NoTemperatureMockProvider;
        assert_eq!(no_temp.count_tokens("anything", "any-model"), 4);
        assert_eq!(no_temp.max_context_tokens("any-model"), 100_000);
        assert_eq!(no_temp.name(), "no-temp-mock");
    }
}
