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
        self.message_tx.send(msg).map_err(|e| {
            ProviderError::Other(format!("Failed to send message: {}", e))
        })
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
    fn deliver_inbox_messages(&mut self) {
        let mut deliveries: Vec<(Entity, Vec<AgentMessage>)> = Vec::new();

        let mut query = self.world.query::<(Entity, &mut MessageInbox)>();
        for (entity, mut inbox) in query.iter_mut(&mut self.world) {
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
                    let _ = window.add_to_region(
                        region_name,
                        format!("[Message]: {}", msg.content),
                        tokens,
                    );
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
                        ProviderError::Other("Agent cancelled before producing a response".to_string())
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
                    let tool_name = tool_calls_snapshot.iter()
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

            // After adding tool results, check if context needs eviction + compaction
            if let Some(cc) = compaction_config {
                match self.evict_and_compact(entity, cc).await {
                    Ok(freed) if freed > 0 => {
                        tracing::info!(iteration, tokens_freed = freed, "Auto-eviction during inference loop");
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
        last_response
            .ok_or_else(|| ProviderError::Other("No response generated".to_string()))
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

            let region = window
                .get_region(region_name)
                .ok_or_else(|| {
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
                },
                Message {
                    role: "user".to_string(),
                    content: user_prompt,
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

        tracing::info!(
            region = region_name,
            "Compaction complete"
        );

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

    #[test]
    fn test_engine_creation() {
        let engine = AgentEngine::new();
        assert!(engine.world().entities().len() == 0);
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
}
