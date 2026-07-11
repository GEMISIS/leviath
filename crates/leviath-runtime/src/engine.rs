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

/// Boxed future returned by a type-erased tool executor. See
/// [`AgentEngine::run_inference_loop_filtered`] for why the executor
/// closure is boxed instead of staying generic.
pub type ToolResultsFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Vec<(String, String)>> + Send + 'a>>;

/// Type-erased tool executor: takes a batch of tool calls, returns a boxed
/// future resolving to `(tool_call_id, result)` pairs.
pub type ToolExecutorDyn<'a> =
    dyn FnMut(Vec<leviath_providers::ToolCall>) -> ToolResultsFuture<'a> + Send + 'a;

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

    /// Replace the internal sender with a disconnected one, making the next
    /// `send_message` call fail. Test-only; not compiled in release.
    #[cfg(test)]
    fn poison_sender(&mut self) {
        let (new_tx, _dropped_rx) = mpsc::unbounded_channel();
        self.message_tx = new_tx;
        // _dropped_rx is dropped here → new_tx is immediately disconnected
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

    /// Drain any undelivered messages from an agent's inbox at stage
    /// transitions. Messages that were never delivered (because the stage
    /// didn't accept them) are logged at warn level and discarded rather
    /// than silently accumulating across stages.
    pub fn drain_pending_messages(&mut self, entity: Entity) {
        // First, pull any messages still sitting in the channel
        self.process_messages();

        // Then drain the inbox
        if let Some(mut inbox) = self.world.get_mut::<MessageInbox>(entity) {
            let pending = inbox.drain_all();
            if !pending.is_empty() {
                tracing::warn!(
                    count = pending.len(),
                    "Draining undelivered messages at stage transition"
                );
                for msg in &pending {
                    tracing::debug!(
                        agent_id = %msg.agent_id,
                        content_len = msg.content.len(),
                        priority = msg.priority,
                        "Discarded undelivered message"
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
            // Use per-stage override, then model capability (which provides sensible defaults)
            let output_cap = self
                .world
                .get::<crate::components::InferenceConfig>(entity)
                .and_then(|c| c.max_output_tokens)
                .unwrap_or_else(|| provider.capabilities(model).max_output_tokens);
            let max_tokens = remaining.min(output_cap);
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

        // Use per-stage temperature override if set, otherwise default to 0.7.
        // Respect each model's temperature support (e.g. claude-opus-4-8 deprecates it).
        let temperature = if provider.capabilities(model).supports_temperature {
            self.world
                .get::<crate::components::InferenceConfig>(entity)
                .and_then(|c| c.temperature)
                .unwrap_or(0.7)
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
        F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut + Send,
        Fut: std::future::Future<
                Output = Vec<(String, String)>, // Vec<(tool_call_id, result)>
            > + Send,
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
    ///
    /// This is a thin, generic wrapper: it boxes `tool_executor` into a
    /// trait-object closure and immediately delegates to
    /// [`Self::run_inference_loop_filtered_dyn`], which contains the entire
    /// actual loop body as a *single, non-generic* function.
    ///
    /// This split exists purely for coverage measurement, not behavior.
    /// `cargo-llvm-cov` instruments each monomorphization of a generic
    /// function separately; this function used to contain the whole loop
    /// body directly, and with ~15-20 call sites (each passing a distinct
    /// closure type) that meant ~15-20 separate coverage-mapping instances
    /// of the same branches. Even though every branch was covered by the
    /// union of all tests, llvm-cov would occasionally report a region as
    /// uncovered for one instantiation, undercounting real coverage. Moving
    /// the logic into one non-generic method compiles it (and instruments
    /// it) exactly once, regardless of how many distinct closure types call
    /// through this wrapper.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_inference_loop_filtered<'p, F, Fut>(
        &mut self,
        entity: Entity,
        provider_name: &str,
        model: &str,
        tools: Vec<Tool>,
        max_iterations: usize,
        tool_filter: Option<&[String]>,
        tool_result_routing: Option<&ToolResultRoutingConfig>,
        compaction_config: Option<&leviath_core::CompactionConfig>,
        tool_executor: &'p mut F,
    ) -> std::result::Result<InferenceResponse, ProviderError>
    where
        F: FnMut(Vec<leviath_providers::ToolCall>) -> Fut + Send,
        Fut: std::future::Future<
                Output = Vec<(String, String)>, // Vec<(tool_call_id, result)>
            > + Send
            + 'p,
    {
        let mut boxed_executor =
            move |tool_calls: Vec<leviath_providers::ToolCall>| -> ToolResultsFuture<'p> {
                Box::pin(tool_executor(tool_calls))
            };

        self.run_inference_loop_filtered_dyn(
            entity,
            provider_name,
            model,
            tools,
            max_iterations,
            tool_filter,
            tool_result_routing,
            compaction_config,
            &mut boxed_executor,
        )
        .await
    }

    /// Non-generic core of [`Self::run_inference_loop_filtered`]. See that
    /// method's doc comment for why this split exists.
    #[allow(clippy::too_many_arguments)]
    /// Type-erased core of [`Self::run_inference_loop_filtered`]. Exposed so
    /// callers that already hold a boxed [`ToolExecutorDyn`] (e.g. the CLI's
    /// single-monomorphization stage loop) can invoke it without re-boxing
    /// through the generic wrapper.
    pub async fn run_inference_loop_filtered_dyn<'e>(
        &mut self,
        entity: Entity,
        provider_name: &str,
        model: &str,
        tools: Vec<Tool>,
        max_iterations: usize,
        tool_filter: Option<&[String]>,
        tool_result_routing: Option<&ToolResultRoutingConfig>,
        compaction_config: Option<&leviath_core::CompactionConfig>,
        tool_executor: &mut ToolExecutorDyn<'e>,
    ) -> std::result::Result<InferenceResponse, ProviderError> {
        let mut last_response = None;
        let mut total_tool_calls: usize = 0;
        let mut text_only_nudges: usize = 0;
        const MAX_TEXT_ONLY_NUDGES: usize = 3;

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
                if total_tool_calls > 0 || text_only_nudges >= MAX_TEXT_ONLY_NUDGES {
                    // Agent has done real work and is finishing, or we've
                    // exhausted nudge attempts — accept the text response.
                    tracing::info!(
                        iteration,
                        total_tool_calls,
                        text_only_nudges,
                        finish_reason = ?response.finish_reason,
                        "Inference loop complete"
                    );
                    return Ok(response);
                }

                // No tool calls yet — model responded with text only (e.g.
                // asking a clarifying question or explaining its plan).
                // Add the text to conversation and nudge it to use tools.
                text_only_nudges += 1;
                tracing::warn!(
                    iteration,
                    text_only_nudges,
                    content_len = response.content.len(),
                    "Model responded with text only before making any tool calls, nudging to use tools ({}/{})",
                    text_only_nudges,
                    MAX_TEXT_ONLY_NUDGES,
                );
                let mut window = self.world.get_mut::<ContextWindow>(entity).unwrap();
                let response_tokens = response.content.len() / 4 + 1;
                let _ = window.add_to_region(
                    "conversation",
                    format!("Assistant: {}", response.content),
                    response_tokens,
                );
                let nudge = "You have tools available. Please use them to complete the task. Start by reading the relevant files in the working directory.";
                let nudge_tokens = nudge.len() / 4 + 1;
                let _ =
                    window.add_to_region("conversation", format!("User: {}", nudge), nudge_tokens);
                last_response = Some(response);
                continue;
            }

            // Execute tool calls
            let tool_calls_snapshot = response.tool_calls.clone();
            total_tool_calls += tool_calls_snapshot.len();
            let tool_results = tool_executor(tool_calls_snapshot.clone()).await;

            // Add tool results to context window.
            // Safety: `run_inference_filtered` already confirmed the entity has
            // a ContextWindow; it cannot be removed between that call and here.
            let mut window = self.world.get_mut::<ContextWindow>(entity).unwrap();

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
            let _ = window; // release borrow before process_messages

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

        // Find the paired CompactHistory region and store summary.
        // Safety: the entity has a ContextWindow — we confirmed it above when
        // reading the region content; it cannot disappear across this await.
        let mut window = self.world.get_mut::<ContextWindow>(entity).unwrap();

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

        // Clear the compacting region (it exists — we read from it above).
        window.get_region_mut(region_name).unwrap().clear();
        window.current_tokens = window.calculate_tokens();

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
    use crate::test_support::with_tracing;

    /// Async-safe variant of `with_tracing`: unlike `tracing::subscriber::
    /// with_default` (a thread-local override scoped to a synchronous
    /// closure), `test_support::with_tracing`'s `set_global_default` install
    /// is process-wide and persists for the lifetime of the test binary, so
    /// simply triggering that one-time install before `.await`-ing the
    /// future correctly covers every await point, even across a
    /// multi-threaded runtime.
    async fn with_tracing_async<T>(f: impl std::future::Future<Output = T>) -> T {
        with_tracing(|| {});
        f.await
    }

    async fn noop_tool_exec(
        _tool_calls: Vec<leviath_providers::ToolCall>,
    ) -> Vec<(String, String)> {
        vec![]
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
        assert_eq!(inbox.messages.len(), 1);

        // Context should be empty
        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = window.get_region("conversation").unwrap();
        assert!(conv.content.is_empty());
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
        assert!(conv.content[0].content.starts_with("User: "));
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
        // Spawn a SECOND agent first so the loop must skip it before matching "cancel-me"
        engine.world_mut().spawn((
            AgentState {
                agent_id: "not-me".to_string(),
                current_stage: "main".to_string(),
                iteration: 0,
                status: AgentStatus::Active,
                spawned_children_ids: Vec::new(),
                pending_wait: None,
                accepts_messages: true,
            },
            CancellationToken::new(),
        ));
        let token = CancellationToken::new();
        let entity = engine
            .world_mut()
            .spawn((
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
            ))
            .id();

        let result = with_tracing(|| engine.cancel_agent("cancel-me"));
        assert!(result.is_ok());

        // Verify status changed
        let state = engine.world.get::<AgentState>(entity).unwrap();
        assert_eq!(state.status, AgentStatus::Cancelled);
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
        assert!(result.unwrap() > 0);
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

        engine.evict_and_compact(entity, &cc).await.unwrap();

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
        assert!(err_msg.to_lowercase().contains("pinned"));
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
        with_tracing_async(engine.run_inference_loop_filtered(
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
        .await
        .unwrap();
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
        result.unwrap();
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
            noop_tool_exec,
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
            .run_inference_loop(entity, "mock", "test-model", Vec::new(), 10, noop_tool_exec)
            .await;
        // Should return error since cancelled before any response
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("cancelled"));

        // Status should be Cancelled
        let state = engine.world().get::<AgentState>(entity).unwrap();
        assert_eq!(state.status, AgentStatus::Cancelled);
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
            .run_inference_loop(entity, "mock", "test-model", Vec::new(), 10, noop_tool_exec)
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

        let result = with_tracing_async(engine.compact_region(entity, "conversation", &cc)).await;
        assert!(result.is_ok());

        // After compaction, conversation should be cleared
        let w = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = w.get_region("conversation").unwrap();
        assert!(conv.content.is_empty());

        // History should have the summary
        let hist = w.get_region("conversation_history").unwrap();
        assert!(!hist.content.is_empty());
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
        assert!(!scratch.content.is_empty());
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
        assert!(!tool_results.content.is_empty());
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
        assert!(!bash_output.content.is_empty());
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

    // ─── FailingMockProvider ───────────────────────────────────────────────
    // Used to cover inference error paths.

    struct FailingMockProvider;

    #[async_trait::async_trait]
    impl leviath_providers::Provider for FailingMockProvider {
        async fn infer(
            &self,
            _req: InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            Err(leviath_providers::ProviderError::Other(
                "intentional test failure".to_string(),
            ))
        }
        fn name(&self) -> &str {
            "failing-mock"
        }
        fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len() / 4 + 1
        }
        fn max_context_tokens(&self, _model: &str) -> usize {
            100_000
        }
        fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    // ─── send_message with dropped receiver ───────────────────────────────

    #[test]
    fn send_message_with_dropped_receiver_returns_err() {
        let mut engine = AgentEngine::new();
        engine.poison_sender(); // replaces tx with a disconnected one
        let msg = AgentMessage {
            agent_id: "x".to_string(),
            content: "hi".to_string(),
            target_region: None,
            priority: 0,
        };
        assert!(engine.send_message(msg).is_err());
    }

    // ─── process_messages with non-matching agent ─────────────────────────

    #[test]
    fn process_messages_logs_warning_for_unknown_agent_id() {
        let mut engine = AgentEngine::new();
        // Spawn an agent with id "known"
        engine.world_mut().spawn((
            AgentState {
                agent_id: "known".to_string(),
                current_stage: "main".to_string(),
                iteration: 0,
                status: AgentStatus::Active,
                spawned_children_ids: Vec::new(),
                pending_wait: None,
                accepts_messages: true,
            },
            MessageInbox::new(),
        ));

        // Send a message targeting "unknown" via the public sender — process_messages must warn
        engine
            .get_message_sender()
            .send(AgentMessage {
                agent_id: "unknown".to_string(),
                content: "hello".to_string(),
                target_region: None,
                priority: 0,
            })
            .unwrap();

        with_tracing(|| engine.process_messages());
    }

    // ─── deliver_inbox_messages without ContextWindow ─────────────────────

    #[test]
    fn deliver_inbox_messages_skips_entity_without_context_window() {
        let mut engine = AgentEngine::new();
        // Spawn entity with inbox but NO ContextWindow
        let mut inbox = MessageInbox::new();
        inbox.push(AgentMessage {
            agent_id: "no-window".to_string(),
            content: "orphan msg".to_string(),
            target_region: None,
            priority: 0,
        });
        engine.world_mut().spawn((
            AgentState {
                agent_id: "no-window".to_string(),
                current_stage: "main".to_string(),
                iteration: 0,
                status: AgentStatus::Active,
                spawned_children_ids: Vec::new(),
                pending_wait: None,
                accepts_messages: true,
            },
            inbox,
        ));
        // deliver_inbox_messages must not panic even without a ContextWindow
        engine.deliver_inbox_messages();
    }

    // ─── cancel_agent for non-existent agent returns Err ─────────────────

    #[test]
    fn cancel_agent_for_unknown_id_returns_err() {
        let mut engine = AgentEngine::new();
        let result = engine.cancel_agent("does-not-exist");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    // ─── run_inference_filtered with failing provider ─────────────────────

    #[tokio::test]
    async fn run_inference_filtered_propagates_provider_error() {
        let mut registry = ProviderRegistry::new();
        registry.register("failing".to_string(), Arc::new(FailingMockProvider));
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            9000,
        ));
        let _ = window.add_to_region("conversation", "User: hi".to_string(), 2);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "fail-agent".to_string(),
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
            .run_inference_filtered(entity, "failing", "test-model", Vec::new(), None)
            .await;
        assert!(result.is_err());
    }

    // ─── run_inference_loop max_iterations=0 (no response generated) ──────

    #[tokio::test]
    async fn run_inference_loop_with_zero_max_iterations_returns_err() {
        let mut registry = ProviderRegistry::new();
        registry.register("mock".to_string(), Arc::new(MockProvider::new("mock")));
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            9000,
        ));
        let _ = window.add_to_region("conversation", "User: hi".to_string(), 2);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "zero-iter".to_string(),
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

        // max_iterations=0 means the loop body never runs, last_response=None,
        // ok_or_else closure fires → Err("No response generated")
        let result = engine
            .run_inference_loop_filtered(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                0,
                None,
                None,
                None,
                &mut noop_tool_exec,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No response"));
    }

    // ─── compact_region without registered provider ────────────────────────

    #[tokio::test]
    async fn compact_region_returns_err_when_provider_not_registered() {
        let mut engine = AgentEngine::new();

        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "analysis".to_string(),
            leviath_core::RegionKind::CompactHistory {
                source_region: "analysis".to_string(),
            },
            5000,
        ));
        let _ = window.add_to_region("analysis", "some content".to_string(), 10);

        let entity = engine.world_mut().spawn(window).id();

        let cc = leviath_core::CompactionConfig {
            provider: "nonexistent".to_string(),
            model: "model".to_string(),
            max_summary_tokens: 500,
            temperature: 0.3,
            system_prompt: None,
            user_prompt_template: None,
        };

        let result = engine.compact_region(entity, "analysis", &cc).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not registered"));
    }

    // ─── compact_region with entity having no ContextWindow ───────────────

    #[tokio::test]
    async fn compact_region_returns_err_when_entity_has_no_context_window() {
        let mut registry = ProviderRegistry::new();
        registry.register("mock".to_string(), Arc::new(MockProvider::new("mock")));
        let mut engine = AgentEngine::with_providers(registry);

        // Entity without ContextWindow
        let entity = engine
            .world_mut()
            .spawn(AgentState {
                agent_id: "no-window".to_string(),
                current_stage: "main".to_string(),
                iteration: 0,
                status: AgentStatus::Active,
                spawned_children_ids: Vec::new(),
                pending_wait: None,
                accepts_messages: true,
            })
            .id();

        let cc = leviath_core::CompactionConfig {
            provider: "mock".to_string(),
            model: "model".to_string(),
            max_summary_tokens: 500,
            temperature: 0.3,
            system_prompt: None,
            user_prompt_template: None,
        };

        let result = engine.compact_region(entity, "analysis", &cc).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no ContextWindow"));
    }

    // ─── compact_region with failing provider (inference fails) ───────────

    #[tokio::test]
    async fn compact_region_propagates_inference_error() {
        let mut registry = ProviderRegistry::new();
        registry.register("failing".to_string(), Arc::new(FailingMockProvider));
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "analysis".to_string(),
            leviath_core::RegionKind::Temporary,
            5000,
        ));
        let _ = window.add_to_region("analysis", "some long content".to_string(), 10);

        let entity = engine.world_mut().spawn(window).id();

        let cc = leviath_core::CompactionConfig {
            provider: "failing".to_string(),
            model: "model".to_string(),
            max_summary_tokens: 500,
            temperature: 0.3,
            system_prompt: None,
            user_prompt_template: None,
        };

        let result = engine.compact_region(entity, "analysis", &cc).await;
        assert!(result.is_err());
    }

    // ─── add_to_region error path (TokenBudgetExceeded) ───────────────────
    // This covers components.rs:149 (the `?` propagation in add_to_region).

    #[test]
    fn add_to_region_returns_err_when_token_budget_exceeded() {
        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "tiny".to_string(),
            leviath_core::RegionKind::Pinned,
            5, // only 5 tokens allowed
        ));
        // Try to add 100 tokens — exceeds the region budget
        let result = window.add_to_region("tiny", "content".to_string(), 100);
        assert!(result.is_err());
    }

    // ─── Cancellation with entity having no AgentState ────────────────────

    #[tokio::test]
    async fn run_inference_loop_cancelled_entity_without_agent_state() {
        let mut registry = ProviderRegistry::new();
        registry.register("mock".to_string(), Arc::new(MockProvider::new("mock")));
        let mut engine = AgentEngine::with_providers(registry);

        // Entity with CancellationToken + ContextWindow but NO AgentState
        let token = CancellationToken::new();
        token.cancel(); // pre-cancel so the loop hits the cancellation check
        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            9000,
        ));
        let _ = window.add_to_region("conversation", "User: hi".to_string(), 2);

        let entity = engine.world_mut().spawn((token, window)).id();

        // max_iterations=1 so the loop tries once; cancellation triggers at top
        let result = engine
            .run_inference_loop_filtered(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                1,
                None,
                None,
                None,
                &mut noop_tool_exec,
            )
            .await;
        // Cancelled before any response → Err
        assert!(result.is_err());
    }

    // ─── Inference fails inside run_inference_loop_filtered ───────────────

    #[tokio::test]
    async fn run_inference_loop_filtered_propagates_inference_error() {
        let mut registry = ProviderRegistry::new();
        registry.register("failing".to_string(), Arc::new(FailingMockProvider));
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow { max_items: 50 },
            9000,
        ));
        let _ = window.add_to_region("conversation", "User: hi".to_string(), 2);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "fail-loop".to_string(),
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
            .run_inference_loop_filtered(
                entity,
                "failing",
                "test-model",
                Vec::new(),
                5,
                None,
                None,
                None,
                &mut noop_tool_exec,
            )
            .await;
        assert!(result.is_err());
    }

    // ─── Routing with short result (no truncation) ────────────────────────
    // Covers the `if result_text.len() > max_chars { ... }` FALSE path.

    #[tokio::test]
    async fn run_inference_loop_filtered_routing_with_short_result_no_truncation() {
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
                    agent_id: "short-result".to_string(),
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
            max_result_tokens: Some(1000), // 4000 chars — much larger than "ok"
        };

        engine
            .run_inference_loop_filtered(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                10,
                None,
                Some(&routing),
                None,
                &mut |_| async {
                    vec![("call_1".to_string(), "ok".to_string())] // "ok" is short → no truncation
                },
            )
            .await
            .unwrap();
    }

    // ─── evict_and_compact where compact_region fails ─────────────────────

    #[tokio::test]
    async fn evict_and_compact_propagates_compact_region_error() {
        let mut registry = ProviderRegistry::new();
        registry.register("failing".to_string(), Arc::new(FailingMockProvider));
        let mut engine = AgentEngine::with_providers(registry);

        // Build a window that's >90% full with a Compacting region so
        // try_evict reaches Phase 3 and adds it to needs_compaction.
        // No Temporary/Clearable regions → eviction can't free space → Phase 3 triggers.
        let mut window = ContextWindow::new(100);

        let mut compacting = leviath_core::Region::new(
            "notes".to_string(),
            leviath_core::RegionKind::Compacting {
                threshold_tokens: 1,
            },
            100,
        );
        // Add 95 tokens so window is 95% full (>90% threshold) and region.needs_compaction()
        compacting
            .add_entry("lots of content here".to_string(), 95)
            .unwrap();
        window.add_region(compacting);
        window.current_tokens = 95;

        let entity = engine.world_mut().spawn(window).id();

        let cc = leviath_core::CompactionConfig {
            provider: "failing".to_string(),
            model: "model".to_string(),
            max_summary_tokens: 20,
            temperature: 0.0,
            system_prompt: None,
            user_prompt_template: None,
        };

        // evict_and_compact will identify "notes" as needing compaction, call
        // compact_region("notes", ...) which fails → propagates Err via `?`
        let result = engine.evict_and_compact(entity, &cc).await;
        assert!(result.is_err());
    }

    // ─── FailingMockProvider trivial methods ──────────────────────────────

    #[test]
    fn failing_mock_provider_trivial_methods() {
        use leviath_providers::Provider;
        let p = FailingMockProvider;
        assert_eq!(p.name(), "failing-mock");
        assert_eq!(p.count_tokens("hi", "m"), 1);
        assert_eq!(p.max_context_tokens("m"), 100_000);
        let _ = p.capabilities("m");
    }

    // ─── drain_pending_messages ────────────────────────────────────────────

    #[tokio::test]
    async fn drain_pending_messages_clears_inbox() {
        let (mut engine, entity) = make_engine_with_mock();
        let tx = engine.get_message_sender();

        // Send some messages but don't process them
        let _ = tx.send(AgentMessage {
            agent_id: "test-mock".to_string(),
            content: "hello".to_string(),
            target_region: None,
            priority: 10,
        });
        let _ = tx.send(AgentMessage {
            agent_id: "test-mock".to_string(),
            content: "world".to_string(),
            target_region: None,
            priority: 5,
        });

        // Disable message acceptance so messages stay in inbox
        engine
            .world_mut()
            .get_mut::<AgentState>(entity)
            .unwrap()
            .accepts_messages = false;

        // drain_pending_messages should pull from channel and clear inbox
        engine.drain_pending_messages(entity);

        // Inbox should be empty now
        let inbox = engine.world().get::<MessageInbox>(entity).unwrap();
        assert!(inbox.messages.is_empty());
    }

    #[tokio::test]
    async fn drain_pending_messages_noop_when_empty() {
        let (mut engine, entity) = make_engine_with_mock();
        // Should not panic when inbox is empty
        engine.drain_pending_messages(entity);
        let inbox = engine.world().get::<MessageInbox>(entity).unwrap();
        assert!(inbox.messages.is_empty());
    }

    // ─── InferenceConfig: temperature + max_output_tokens override ──────

    #[tokio::test]
    async fn inference_config_temperature_override() {
        let (mut engine, entity) = make_engine_with_mock();
        engine
            .providers_mut()
            .register("no-temp".to_string(), Arc::new(NoTemperatureMockProvider));

        // Set a temperature override of 0.3
        engine
            .world_mut()
            .entity_mut(entity)
            .insert(crate::components::InferenceConfig {
                temperature: Some(0.3),
                max_output_tokens: None,
            });

        // NoTemperatureMockProvider echoes the temperature, but its caps say
        // supports_temperature=false, so the override is ignored and 0.0 is used.
        let result = engine
            .run_inference(entity, "no-temp", "reasoning-model", Vec::new())
            .await
            .unwrap();
        assert_eq!(result.content, "temperature=0");

        // Now test with a provider that supports temperature — use a provider
        // that echoes the temperature.
        struct TempEchoProvider;
        #[async_trait::async_trait]
        impl leviath_providers::Provider for TempEchoProvider {
            async fn infer(
                &self,
                request: InferenceRequest,
            ) -> leviath_providers::Result<InferenceResponse> {
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
                "temp-echo"
            }
            fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
                leviath_providers::ModelCapabilities::default()
            }
        }

        engine
            .providers_mut()
            .register("temp-echo".to_string(), Arc::new(TempEchoProvider));

        let result = engine
            .run_inference(entity, "temp-echo", "test-model", Vec::new())
            .await
            .unwrap();
        // Should use our 0.3 override
        assert!(
            result.content.contains("0.3"),
            "Expected temperature 0.3, got: {}",
            result.content
        );

        // Cover the mock's trivial trait methods, which infer()-driven tests
        // never reach on their own.
        let probe = TempEchoProvider;
        assert_eq!(
            leviath_providers::Provider::count_tokens(&probe, "x", "m"),
            4
        );
        assert_eq!(
            leviath_providers::Provider::max_context_tokens(&probe, "m"),
            100_000
        );
        assert_eq!(leviath_providers::Provider::name(&probe), "temp-echo");
    }

    #[tokio::test]
    async fn inference_config_temperature_default_when_no_override() {
        let (mut engine, entity) = make_engine_with_mock();

        struct TempEchoProvider;
        #[async_trait::async_trait]
        impl leviath_providers::Provider for TempEchoProvider {
            async fn infer(
                &self,
                request: InferenceRequest,
            ) -> leviath_providers::Result<InferenceResponse> {
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
                "temp-echo"
            }
            fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
                leviath_providers::ModelCapabilities::default()
            }
        }

        engine
            .providers_mut()
            .register("temp-echo".to_string(), Arc::new(TempEchoProvider));

        // No InferenceConfig on entity — should default to 0.7
        let result = engine
            .run_inference(entity, "temp-echo", "test-model", Vec::new())
            .await
            .unwrap();
        assert!(
            result.content.contains("0.7"),
            "Expected default temperature 0.7, got: {}",
            result.content
        );

        let probe = TempEchoProvider;
        assert_eq!(
            leviath_providers::Provider::count_tokens(&probe, "x", "m"),
            4
        );
        assert_eq!(
            leviath_providers::Provider::max_context_tokens(&probe, "m"),
            100_000
        );
        assert_eq!(leviath_providers::Provider::name(&probe), "temp-echo");
    }

    #[tokio::test]
    async fn inference_config_max_output_tokens_override() {
        struct MaxTokensEchoProvider;
        #[async_trait::async_trait]
        impl leviath_providers::Provider for MaxTokensEchoProvider {
            async fn infer(
                &self,
                request: InferenceRequest,
            ) -> leviath_providers::Result<InferenceResponse> {
                Ok(InferenceResponse {
                    content: format!("max_tokens={}", request.max_tokens),
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
                "max-echo"
            }
            fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
                leviath_providers::ModelCapabilities {
                    max_output_tokens: 8192,
                    ..Default::default()
                }
            }
        }

        let mut registry = ProviderRegistry::new();
        registry.register("max-echo".to_string(), Arc::new(MaxTokensEchoProvider));
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "system".to_string(),
            leviath_core::RegionKind::Pinned,
            2000,
        ));
        let _ = window.add_to_region("system", "prompt".to_string(), 6);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "test".to_string(),
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

        // Without InferenceConfig, should use model capability (8192)
        let result = engine
            .run_inference(entity, "max-echo", "test-model", Vec::new())
            .await
            .unwrap();
        assert!(
            result.content.contains("8192"),
            "Expected max_output_tokens from capability (8192), got: {}",
            result.content
        );

        // With InferenceConfig override to 2048
        engine
            .world_mut()
            .entity_mut(entity)
            .insert(crate::components::InferenceConfig {
                temperature: None,
                max_output_tokens: Some(2048),
            });
        let result = engine
            .run_inference(entity, "max-echo", "test-model", Vec::new())
            .await
            .unwrap();
        assert!(
            result.content.contains("2048"),
            "Expected max_output_tokens override (2048), got: {}",
            result.content
        );

        let probe = MaxTokensEchoProvider;
        assert_eq!(
            leviath_providers::Provider::count_tokens(&probe, "x", "m"),
            4
        );
        assert_eq!(
            leviath_providers::Provider::max_context_tokens(&probe, "m"),
            100_000
        );
        assert_eq!(leviath_providers::Provider::name(&probe), "max-echo");
    }
}
