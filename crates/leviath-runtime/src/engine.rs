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

/// Truncate `text` to at most `max_chars` characters, cutting only on UTF-8
/// char boundaries.
///
/// Tool results — especially batch reads like `read_files`, which concatenate
/// many UTF-8 files — routinely need truncating to fit a region's token budget.
/// Byte-indexed truncation (`String::truncate` / `&s[..n]`) panics when the cut
/// lands inside a multi-byte character; taking whole `char`s never does. This
/// mirrors the char-safe idiom used on the worker side (`worker.rs`). `max_chars`
/// is an approximate char budget (the caller derives it from a token estimate).
fn truncate_on_char_boundary(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

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

    /// Taint gate for the current stage, when taint tracking is active. `None`
    /// means no enforcement (the common/default case). Reconfigured per stage
    /// by the CLI via [`AgentEngine::configure_taint`].
    taint_gate: Option<crate::taint::TaintGate>,

    /// Policy (allowlists / MCP overrides) consulted when the gate blocks.
    taint_policy: leviath_core::PolicyConfig,

    /// Injected resolver used to interactively decide a blocked outbound call.
    /// `None` → blocked calls are denied outright.
    gate_prompt: Option<Box<dyn crate::taint::GatePrompt>>,
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

/// Sync callback invoked after tool execution when context tools were used.
/// Receives the engine's World and the agent Entity so callers can sync
/// external state (e.g. a shared ContextWindow copy) back to the entity.
pub type PostToolSync = dyn FnMut(&mut bevy_ecs::prelude::World, bevy_ecs::prelude::Entity) + Send;

/// Shared, concurrently-drivable handle to an [`AgentEngine`].
///
/// The stage loop and all in-process sub-agents share one of these so multiple
/// agents' inference loops can run concurrently over a single ECS `World`.
/// Drive an agent through it with [`run_inference_loop_shared`].
pub type EngineHandle = Arc<tokio::sync::RwLock<AgentEngine>>;

/// Outcome of [`AgentEngine::loop_handle_empty_tool_calls`] — the inference
/// loop either finishes or injects a nudge and keeps looping.
enum EmptyToolCallsOutcome {
    /// The agent finished (did real work earlier, or exhausted nudge attempts).
    Finish,
    /// The agent produced text only; a nudge was injected — keep looping.
    Nudged,
}

/// Accumulate one inference call's token usage into a running total.
fn accumulate_tokens(
    cumulative: &mut leviath_providers::TokenUsage,
    used: &leviath_providers::TokenUsage,
) {
    cumulative.prompt_tokens += used.prompt_tokens;
    cumulative.completion_tokens += used.completion_tokens;
    cumulative.total_tokens += used.total_tokens;
    cumulative.cached_tokens += used.cached_tokens;
    cumulative.cache_write_tokens += used.cache_write_tokens;
}

/// Drive an agent's inference loop through a shared [`EngineHandle`].
///
/// This is the unified entry point used by both the root agent and in-process
/// sub-agents. It runs the same per-iteration critical sections as
/// [`AgentEngine::run_inference_loop_filtered_dyn_with_sync`], but acquires the
/// engine lock only in short bursts and **releases it across every network
/// await** — the main `provider.infer` call and the tool-executor call — so
/// multiple agents' loops overlap their (slow) network work.
///
/// Lock discipline: the guard is never held across `provider.infer` or
/// `tool_executor`. Two awaits are (for now) still run under a guard —
/// `taint_gate_partition`'s interactive prompt (only reached when taint is
/// enabled and a call is blocked) and `evict_and_compact`'s compaction inference
/// (only when compaction triggers). Both are conditional/rare; splitting them
/// out is a follow-up. Neither re-acquires the engine lock, so there is no
/// deadlock.
#[allow(clippy::too_many_arguments)]
pub async fn run_inference_loop_shared<'e>(
    engine: &EngineHandle,
    entity: Entity,
    provider_name: &str,
    model: &str,
    tools: Vec<Tool>,
    max_iterations: usize,
    tool_filter: Option<&[String]>,
    compaction_config: Option<&leviath_core::CompactionConfig>,
    tool_executor: &mut ToolExecutorDyn<'e>,
    repetition_config: Option<&leviath_core::RepetitionDetectionConfig>,
    mut post_tool_sync: Option<&mut PostToolSync>,
) -> std::result::Result<InferenceResponse, ProviderError> {
    let mut last_response: Option<InferenceResponse> = None;
    let mut total_tool_calls: usize = 0;
    let mut text_only_nudges: usize = 0;

    let rep_config = {
        let enabled = repetition_config.and_then(|c| c.enabled).unwrap_or(true);
        let max_repeat = repetition_config
            .and_then(|c| c.max_repeat_calls)
            .unwrap_or(3);
        let max_streak = repetition_config
            .and_then(|c| c.max_readonly_streak)
            .unwrap_or(10);
        crate::repetition::RepetitionConfig {
            max_repeat_calls: max_repeat,
            max_readonly_streak: max_streak,
            enabled,
        }
    };
    let mut repetition_detector = crate::repetition::RepetitionDetector::new(rep_config);
    let mut cumulative_tokens = leviath_providers::TokenUsage {
        prompt_tokens: 0,
        completion_tokens: 0,
        total_tokens: 0,
        cached_tokens: 0,
        cache_write_tokens: 0,
    };

    for iteration in 0..max_iterations {
        // CS: cancellation + message intake (write guard, no await).
        {
            let mut g = engine.write().await;
            if g.loop_check_cancelled(entity, iteration) {
                return last_response
                    .map(|r| InferenceResponse {
                        tokens_used: cumulative_tokens.clone(),
                        ..r
                    })
                    .ok_or_else(|| {
                        ProviderError::Other(
                            "Agent cancelled before producing a response".to_string(),
                        )
                    });
            }
            g.process_messages();
        }

        // CS: assemble the request + clone the provider handle (read guard, no await).
        let (provider, request) = {
            let g = engine.read().await;
            g.build_inference_request(entity, provider_name, model, tools.clone(), tool_filter)?
        };

        // Network call — NO engine lock held. This is what lets workers overlap.
        let response = provider.infer(request).await?;
        accumulate_tokens(&mut cumulative_tokens, &response.tokens_used);

        // CS: store the response on the entity (write guard, no await).
        {
            let mut g = engine.write().await;
            g.apply_inference_response(entity, &response);
        }

        // Handle a response that made no tool calls: finish or nudge.
        if response.tool_calls.is_empty() {
            let outcome = {
                let mut g = engine.write().await;
                g.loop_handle_empty_tool_calls(
                    entity,
                    &response,
                    total_tool_calls,
                    &mut text_only_nudges,
                    iteration,
                    &cumulative_tokens,
                )
            };
            match outcome {
                EmptyToolCallsOutcome::Finish => {
                    return Ok(InferenceResponse {
                        tokens_used: cumulative_tokens,
                        ..response
                    });
                }
                EmptyToolCallsOutcome::Nudged => {
                    last_response = Some(response);
                    continue;
                }
            }
        }

        let tool_calls_snapshot = response.tool_calls.clone();
        total_tool_calls += tool_calls_snapshot.len();

        // CS: taint gate. The guard is held across the (conditional) prompt await
        // — see the lock-discipline note above.
        let (calls_to_execute, mut denied_results) = {
            let mut g = engine.write().await;
            g.taint_gate_partition(entity, &tool_calls_snapshot).await
        };

        // Tool execution — NO engine lock held (executors do I/O, MCP, and may
        // re-enter the engine to spawn/manage sub-agents).
        let mut tool_results = tool_executor(calls_to_execute).await;
        tool_results.append(&mut denied_results);

        // CS: append assistant turn + tool results, repetition nudges, messages.
        {
            let mut g = engine.write().await;
            g.loop_apply_tool_results(
                entity,
                &response,
                &tool_calls_snapshot,
                tool_results,
                post_tool_sync.as_deref_mut(),
                &mut repetition_detector,
                iteration,
            );
        }

        // CS: eviction + compaction. The guard is held across the (conditional)
        // compaction inference — see the lock-discipline note above.
        if let Some(cc) = compaction_config {
            let mut g = engine.write().await;
            match g.evict_and_compact(entity, cc).await {
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

        // CS: post-tool sync (write guard, no await).
        {
            let mut g = engine.write().await;
            g.loop_post_sync(entity, post_tool_sync.as_deref_mut());
        }

        last_response = Some(response);
    }

    tracing::warn!(max_iterations, "Inference loop hit max iterations");
    last_response
        .map(|r| InferenceResponse {
            tokens_used: cumulative_tokens,
            ..r
        })
        .ok_or_else(|| ProviderError::Other("No response generated".to_string()))
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
            taint_gate: None,
            taint_policy: leviath_core::PolicyConfig::default(),
            gate_prompt: None,
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
            taint_gate: None,
            taint_policy: leviath_core::PolicyConfig::default(),
            gate_prompt: None,
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

    /// Configure taint enforcement for the upcoming stage. When set, the
    /// inference loop tags tool results with the tool's declared sensitivity
    /// and gates outbound calls whose data exceeds the tool's clearance,
    /// prompting via `prompt` (or denying outright when `prompt` is `None`).
    pub fn configure_taint(
        &mut self,
        gate: crate::taint::TaintGate,
        policy: leviath_core::PolicyConfig,
        prompt: Option<Box<dyn crate::taint::GatePrompt>>,
    ) {
        self.taint_gate = Some(gate);
        self.taint_policy = policy;
        self.gate_prompt = prompt;
    }

    /// Disable taint enforcement (no tagging, no gating).
    pub fn clear_taint(&mut self) {
        self.taint_gate = None;
        self.gate_prompt = None;
    }

    /// Turn on taint tracking for the given entity's context window (idempotent).
    pub fn enable_entity_taint_tracking(&mut self, entity: bevy_ecs::prelude::Entity) {
        if let Some(mut w) = self.world.get_mut::<ContextWindow>(entity) {
            w.enable_taint_tracking();
        }
    }

    /// The current stage's taint audit log (empty when taint is inactive).
    pub fn taint_audit_log(&self) -> &[leviath_core::taint::GateEvent] {
        self.taint_gate
            .as_ref()
            .map(|g| g.audit_log())
            .unwrap_or(&[])
    }

    /// Partition a batch of tool calls into `(calls_to_execute, denied_results)`
    /// per the taint gate. Outbound calls whose incoming data exceeds the
    /// tool's clearance are resolved via the injected prompt (allow-once /
    /// always-allow / deny); a deny substitutes a `[blocked]` result and skips
    /// execution. When the gate is inactive, every call executes.
    /// Synchronously classify a batch of tool calls against the current taint
    /// state. Extracted from [`Self::taint_gate_partition`] so all the
    /// gate/policy logic lives in a non-async function (fully unit-testable and
    /// not subject to async-instantiation coverage artifacts); the async fn
    /// only awaits the prompt.
    fn gate_decisions(
        &mut self,
        entity: Entity,
        agent_id: &str,
        policy: &leviath_core::PolicyConfig,
        calls: &[leviath_providers::ToolCall],
    ) -> Vec<(
        leviath_providers::ToolCall,
        leviath_core::taint::GateDecision,
    )> {
        let window = self.world.get::<ContextWindow>(entity).unwrap();
        let gate = self.taint_gate.as_mut().unwrap();
        calls
            .iter()
            .map(|tc| {
                let d = gate.check_with_policy(agent_id, &tc.name, window, None, policy, None);
                (tc.clone(), d)
            })
            .collect()
    }

    async fn taint_gate_partition(
        &mut self,
        entity: Entity,
        calls: &[leviath_providers::ToolCall],
    ) -> (Vec<leviath_providers::ToolCall>, Vec<(String, String)>) {
        use crate::taint::GateResolution;

        if !self
            .taint_gate
            .as_ref()
            .map(|g| g.is_enabled())
            .unwrap_or(false)
        {
            return (calls.to_vec(), Vec::new());
        }

        let agent_id = self
            .world
            .get::<AgentState>(entity)
            .map(|s| s.agent_id.clone())
            .unwrap_or_default();
        let policy = self.taint_policy.clone();
        let decisions = self.gate_decisions(entity, &agent_id, &policy, calls);

        // Resolve blocks (the only async step) and partition. All non-async
        // work is delegated to the synchronous `TaintGate::apply_resolution`.
        let mut to_execute = Vec::new();
        let mut denied = Vec::new();
        for (tc, decision) in decisions {
            let Some((taint, clearance)) = decision.blocked_levels() else {
                to_execute.push(tc);
                continue;
            };
            let resolution = match self.gate_prompt.as_ref() {
                Some(prompt) => prompt.resolve(&decision).await,
                None => GateResolution::Deny,
            };
            let outcome = self
                .taint_gate
                .as_mut()
                .unwrap()
                .apply_resolution(&agent_id, &tc.name, &tc.id, taint, clearance, resolution);
            match outcome {
                Some(blocked) => denied.push(blocked),
                None => to_execute.push(tc),
            }
        }
        (to_execute, denied)
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
                    let _ = window.add_typed_entry(
                        region_name,
                        leviath_core::EntryKind::UserMessage,
                        msg.content.clone(),
                        tokens,
                    );
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
    ///
    /// This is the sequential (`&mut self`) form: it builds the request, awaits
    /// the network call, and applies the response, all while holding `&mut self`.
    /// The concurrent driver ([`run_inference_loop_shared`]) instead calls
    /// [`Self::build_inference_request`] / [`Self::apply_inference_response`]
    /// around the network call so it can release the engine lock across the
    /// (lock-free) `provider.infer` await.
    pub async fn run_inference_filtered(
        &mut self,
        entity: Entity,
        provider_name: &str,
        model: &str,
        tools: Vec<Tool>,
        tool_filter: Option<&[String]>,
    ) -> std::result::Result<InferenceResponse, ProviderError> {
        let (provider, request) =
            self.build_inference_request(entity, provider_name, model, tools, tool_filter)?;
        let response = provider.infer(request).await?;
        self.apply_inference_response(entity, &response);
        Ok(response)
    }

    /// Build the [`InferenceRequest`] for an agent and clone its provider handle.
    ///
    /// Pure read of the ECS world (`ContextWindow` + `InferenceConfig`) plus a
    /// provider-registry lookup; performs no `.await`. Splitting this out lets
    /// the concurrent driver assemble the request under a short read/borrow and
    /// then drop the engine lock before the network call.
    pub fn build_inference_request(
        &self,
        entity: Entity,
        provider_name: &str,
        model: &str,
        tools: Vec<Tool>,
        tool_filter: Option<&[String]>,
    ) -> std::result::Result<(Arc<dyn Provider>, InferenceRequest), ProviderError> {
        let provider = self
            .provider_registry
            .get(provider_name)
            .ok_or_else(|| {
                ProviderError::Other(format!("Provider '{}' not registered", provider_name))
            })?
            .clone();

        // Build structured messages from the context window
        let (assembled, max_tokens) = {
            let window = self
                .world
                .get::<ContextWindow>(entity)
                .ok_or_else(|| ProviderError::Other("Entity has no ContextWindow".to_string()))?;

            let assembled = window.assemble();
            let remaining = window.max_tokens.saturating_sub(window.current_tokens);
            // Use per-stage override, then model capability (which provides sensible defaults)
            let output_cap = self
                .world
                .get::<crate::components::InferenceConfig>(entity)
                .and_then(|c| c.max_output_tokens)
                .unwrap_or_else(|| provider.capabilities(model).max_output_tokens);
            let max_tokens = remaining.min(output_cap);
            (assembled, max_tokens)
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
            system: assembled.system_blocks,
            messages: assembled.messages,
            model: model.to_string(),
            max_tokens,
            temperature,
            tools: filtered_tools,
            extra: serde_json::Value::Null,
        };

        Ok((provider, request))
    }

    /// Store an inference response on the entity and bump its iteration counter.
    ///
    /// The write-side counterpart to [`Self::build_inference_request`]; performs
    /// no `.await`, so the concurrent driver can run it under a short write lock
    /// after the network call has completed.
    pub fn apply_inference_response(&mut self, entity: Entity, response: &InferenceResponse) {
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
    }

    /// Inference-loop critical section: check the agent's cancellation token.
    ///
    /// Returns `true` (and marks the agent `Cancelled`) if the loop should stop.
    /// Extracted so both the sequential loop and the concurrent shared driver
    /// run identical logic under their respective borrows/guards.
    fn loop_check_cancelled(&mut self, entity: Entity, iteration: usize) -> bool {
        if let Some(token) = self.world.get::<CancellationToken>(entity) {
            if token.is_cancelled() {
                tracing::info!(iteration, "Inference loop cancelled");
                if let Some(mut state) = self.world.get_mut::<AgentState>(entity) {
                    state.status = AgentStatus::Cancelled;
                }
                return true;
            }
        }
        false
    }

    /// Inference-loop critical section: handle a response that carried no tool
    /// calls — either finish, or inject a "use your tools" nudge and continue.
    fn loop_handle_empty_tool_calls(
        &mut self,
        entity: Entity,
        response: &InferenceResponse,
        total_tool_calls: usize,
        text_only_nudges: &mut usize,
        iteration: usize,
        cumulative: &leviath_providers::TokenUsage,
    ) -> EmptyToolCallsOutcome {
        const MAX_TEXT_ONLY_NUDGES: usize = 3;
        if total_tool_calls > 0 || *text_only_nudges >= MAX_TEXT_ONLY_NUDGES {
            // Agent has done real work and is finishing, or we've
            // exhausted nudge attempts — accept the text response.
            tracing::info!(
                iteration,
                total_tool_calls,
                text_only_nudges = *text_only_nudges,
                cumulative_prompt_tokens = cumulative.prompt_tokens,
                cumulative_completion_tokens = cumulative.completion_tokens,
                finish_reason = ?response.finish_reason,
                "Inference loop complete"
            );
            return EmptyToolCallsOutcome::Finish;
        }

        // No tool calls yet — model responded with text only (e.g.
        // asking a clarifying question or explaining its plan).
        // Add the text to conversation and nudge it to use tools.
        *text_only_nudges += 1;
        tracing::warn!(
            iteration,
            text_only_nudges = *text_only_nudges,
            content_len = response.content.len(),
            "Model responded with text only before making any tool calls, nudging to use tools ({}/{})",
            *text_only_nudges,
            MAX_TEXT_ONLY_NUDGES,
        );
        let mut window = self.world.get_mut::<ContextWindow>(entity).unwrap();
        let response_tokens = response.content.len() / 4 + 1;
        let _ = window.add_typed_entry(
            "conversation",
            leviath_core::EntryKind::AssistantTurn { tool_calls: vec![] },
            response.content.clone(),
            response_tokens,
        );
        let nudge = "You have tools available. Please use them to complete the task. Start by reading the relevant files in the working directory.";
        let nudge_tokens = nudge.len() / 4 + 1;
        let _ = window.add_typed_entry(
            "conversation",
            leviath_core::EntryKind::UserMessage,
            nudge.to_string(),
            nudge_tokens,
        );
        EmptyToolCallsOutcome::Nudged
    }

    /// Inference-loop critical section: append the assistant turn and all tool
    /// results to the agent's context window (with routing, truncation, taint
    /// tagging), feed the repetition detector, and drain pending messages.
    #[allow(clippy::too_many_arguments)]
    fn loop_apply_tool_results(
        &mut self,
        entity: Entity,
        response: &InferenceResponse,
        tool_calls_snapshot: &[leviath_providers::ToolCall],
        tool_results: Vec<(String, String)>,
        mut post_tool_sync: Option<&mut PostToolSync>,
        repetition_detector: &mut crate::repetition::RepetitionDetector,
        iteration: usize,
    ) {
        // When taint tracking is active, precompute each tool's declared
        // output sensitivity (before borrowing the window) so results are
        // tagged as they are routed into regions.
        let tool_sensitivities: Option<HashMap<String, leviath_core::TaintLevel>> = self
            .taint_gate
            .as_ref()
            .filter(|g| g.is_enabled())
            .map(|g| {
                tool_calls_snapshot
                    .iter()
                    .map(|tc| (tc.name.clone(), g.tool_classification(&tc.name).sensitivity))
                    .collect()
            });

        // Sync external state (shared ContextWindow) back to the entity
        // before we add tool results. This covers both context_* tools and
        // file tracking (which writes to the shared CW via read_file/write_file).
        if let Some(sync) = post_tool_sync.as_mut() {
            sync(&mut self.world, entity);
        }

        // Read tool result routing config from entity (if present).
        let tool_result_routing = self
            .world
            .get::<crate::ToolResultRoutingComponent>(entity)
            .map(|c| c.routing.clone());

        // Add tool results to context window.
        // Safety: `run_inference_filtered` already confirmed the entity has
        // a ContextWindow; it cannot be removed between that call and here.
        let mut window = self.world.get_mut::<ContextWindow>(entity).unwrap();

        // Add assistant response with tool calls as a typed entry
        let response_tokens = response.content.len() / 4;
        let serialized_tool_calls: Vec<leviath_core::SerializedToolCall> = tool_calls_snapshot
            .iter()
            .map(|tc| leviath_core::SerializedToolCall {
                id: tc.id.clone(),
                name: tc.name.clone(),
                arguments: tc.arguments.clone(),
            })
            .collect();
        let _ = window.add_typed_entry(
            "conversation",
            leviath_core::EntryKind::AssistantTurn {
                tool_calls: serialized_tool_calls,
            },
            response.content.clone(),
            response_tokens,
        );

        // Add tool results as typed entries in the messages region.
        // These MUST succeed — an AssistantTurn with tool_calls was just
        // added above, and Anthropic requires every tool_use to have a
        // matching tool_result. If the region is at its token budget,
        // truncate the result content rather than silently dropping it.
        for (tool_call_id, result) in &tool_results {
            let mut result_text = result.clone();
            let tool_name = tool_calls_snapshot
                .iter()
                .find(|tc| tc.id == *tool_call_id)
                .map(|tc| tc.name.clone())
                .unwrap_or_default();

            // Apply max_result_tokens truncation from routing config
            if let Some(routing) = tool_result_routing.as_ref() {
                if let Some(max_tokens) = routing.max_result_tokens {
                    let max_chars = max_tokens * 4;
                    if result_text.len() > max_chars {
                        result_text = truncate_on_char_boundary(&result_text, max_chars);
                        result_text.push_str("\n[...truncated]");
                    }
                }
            }

            let result_tokens = result_text.len() / 4 + 1;

            // Determine target region from routing config
            let target_region = if let Some(routing) = tool_result_routing.as_ref() {
                if let Some(override_region) = routing.tool_overrides.get(&tool_name) {
                    override_region.as_str()
                } else {
                    routing.default_region.as_str()
                }
            } else {
                "conversation"
            };

            // Handle persist=false: route to scratch if available
            let target_region = if let Some(routing) = tool_result_routing.as_ref() {
                if !routing.persist {
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

            let kind = leviath_core::EntryKind::ToolResult {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                is_error: false,
            };

            // When taint tracking is enabled, a tool result carries the
            // tool's configured sensitivity level so the taint gate sees
            // sensitive output flow into context; otherwise it's added as a
            // plain typed entry (tracked as Public). Either way it keeps its
            // ToolResult kind so turn-group eviction stays intact.
            let taint_level = tool_sensitivities.as_ref().map(|sens| {
                sens.get(&tool_name)
                    .copied()
                    .unwrap_or(leviath_core::TaintLevel::Public)
            });
            let add_tool_result = |window: &mut ContextWindow,
                                   region: &str,
                                   kind: leviath_core::EntryKind,
                                   content: String,
                                   tokens: usize| {
                match taint_level {
                    Some(level) => {
                        window.add_typed_tainted_to_region(region, kind, content, tokens, level)
                    }
                    None => window.add_typed_entry(region, kind, content, tokens),
                }
            };

            // Try adding the full result first. These MUST succeed — an
            // AssistantTurn with tool_calls was just added above, and
            // Anthropic requires every tool_use to have a matching
            // tool_result. If the region is at its token budget, truncate
            // rather than dropping (which would orphan the tool_use block).
            if add_tool_result(
                &mut window,
                target_region,
                kind.clone(),
                result_text.clone(),
                result_tokens,
            )
            .is_err()
            {
                let available = window
                    .get_region(target_region)
                    .map(|r| r.max_tokens.saturating_sub(r.current_tokens))
                    .unwrap_or(0);

                let truncated = if available > 100 {
                    let char_budget = (available - 10) * 4; // rough tokens→chars
                    let prefix = truncate_on_char_boundary(&result_text, char_budget);
                    let omitted = result_text.len().saturating_sub(prefix.len());
                    format!("{}... [truncated, {} chars omitted]", prefix, omitted)
                } else {
                    "[tool result truncated — context window full]".to_string()
                };
                let trunc_tokens = truncated.len() / 4 + 1;

                if add_tool_result(&mut window, target_region, kind, truncated, trunc_tokens)
                    .is_err()
                {
                    // Last resort: add a minimal placeholder
                    let placeholder = "[result omitted]".to_string();
                    let _ = add_tool_result(
                        &mut window,
                        target_region,
                        leviath_core::EntryKind::ToolResult {
                            tool_call_id: tool_call_id.clone(),
                            tool_name: tool_name.clone(),
                            is_error: false,
                        },
                        placeholder,
                        5,
                    );
                }
            }
        }
        let _ = window; // release borrow before process_messages

        // Feed tool calls into the repetition detector and inject nudges
        for tc in tool_calls_snapshot {
            let args_str = tc.arguments.to_string();
            if let Some(nudge) = repetition_detector.record_call(&tc.name, &args_str) {
                tracing::warn!(
                    tool_name = %tc.name,
                    iteration,
                    "Repetition detected, injecting nudge"
                );
                let mut window = self.world.get_mut::<ContextWindow>(entity).unwrap();
                let nudge_tokens = nudge.len() / 4 + 1;
                let _ = window.add_typed_entry(
                    "conversation",
                    leviath_core::EntryKind::UserMessage,
                    nudge,
                    nudge_tokens,
                );
            }
        }

        // Check for any user messages that arrived during tool execution
        self.process_messages();
    }

    /// Inference-loop critical section: sync entity→shared context after tool
    /// results and eviction, so context tools / file tracking see the latest
    /// state on the next iteration.
    fn loop_post_sync(&mut self, entity: Entity, mut post_tool_sync: Option<&mut PostToolSync>) {
        if let Some(sync) = post_tool_sync.as_mut() {
            sync(&mut self.world, entity);
        }
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
            &mut tool_executor,
        )
        .await
    }

    /// Run the full inference loop with optional tool filtering.
    ///
    /// `tool_filter`: if Some, only tools matching these names are included.
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
        compaction_config: Option<&leviath_core::CompactionConfig>,
        tool_executor: &mut ToolExecutorDyn<'e>,
    ) -> std::result::Result<InferenceResponse, ProviderError> {
        self.run_inference_loop_filtered_dyn_inner(
            entity,
            provider_name,
            model,
            tools,
            max_iterations,
            tool_filter,
            compaction_config,
            tool_executor,
            None,
        )
        .await
    }

    /// Inner implementation of [`Self::run_inference_loop_filtered_dyn`] with
    /// optional repetition detection configuration.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_inference_loop_filtered_dyn_inner<'e>(
        &mut self,
        entity: Entity,
        provider_name: &str,
        model: &str,
        tools: Vec<Tool>,
        max_iterations: usize,
        tool_filter: Option<&[String]>,
        compaction_config: Option<&leviath_core::CompactionConfig>,
        tool_executor: &mut ToolExecutorDyn<'e>,
        repetition_config: Option<&leviath_core::RepetitionDetectionConfig>,
    ) -> std::result::Result<InferenceResponse, ProviderError> {
        self.run_inference_loop_filtered_dyn_with_sync(
            entity,
            provider_name,
            model,
            tools,
            max_iterations,
            tool_filter,
            compaction_config,
            tool_executor,
            repetition_config,
            None,
        )
        .await
    }

    /// Like [`Self::run_inference_loop_filtered_dyn_inner`] but with an
    /// optional post-tool-execution sync callback.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_inference_loop_filtered_dyn_with_sync<'e>(
        &mut self,
        entity: Entity,
        provider_name: &str,
        model: &str,
        tools: Vec<Tool>,
        max_iterations: usize,
        tool_filter: Option<&[String]>,
        compaction_config: Option<&leviath_core::CompactionConfig>,
        tool_executor: &mut ToolExecutorDyn<'e>,
        repetition_config: Option<&leviath_core::RepetitionDetectionConfig>,
        mut post_tool_sync: Option<&mut PostToolSync>,
    ) -> std::result::Result<InferenceResponse, ProviderError> {
        let mut last_response = None;
        let mut total_tool_calls: usize = 0;
        let mut text_only_nudges: usize = 0;

        // Set up repetition detector from config
        let rep_config = {
            let enabled = repetition_config.and_then(|c| c.enabled).unwrap_or(true);
            let max_repeat = repetition_config
                .and_then(|c| c.max_repeat_calls)
                .unwrap_or(3);
            let max_streak = repetition_config
                .and_then(|c| c.max_readonly_streak)
                .unwrap_or(10);
            crate::repetition::RepetitionConfig {
                max_repeat_calls: max_repeat,
                max_readonly_streak: max_streak,
                enabled,
            }
        };
        let mut repetition_detector = crate::repetition::RepetitionDetector::new(rep_config);
        let mut cumulative_tokens = leviath_providers::TokenUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
        };

        for iteration in 0..max_iterations {
            // Check cancellation token before each iteration
            if self.loop_check_cancelled(entity, iteration) {
                return last_response
                    .map(|r| InferenceResponse {
                        tokens_used: cumulative_tokens.clone(),
                        ..r
                    })
                    .ok_or_else(|| {
                        ProviderError::Other(
                            "Agent cancelled before producing a response".to_string(),
                        )
                    });
            }

            // Process any pending messages before inference
            self.process_messages();

            tracing::debug!(iteration, "Inference loop iteration");

            let response = self
                .run_inference_filtered(entity, provider_name, model, tools.clone(), tool_filter)
                .await?;

            // Accumulate token usage across all iterations
            accumulate_tokens(&mut cumulative_tokens, &response.tokens_used);

            // Check if we're done (no tool calls)
            if response.tool_calls.is_empty() {
                match self.loop_handle_empty_tool_calls(
                    entity,
                    &response,
                    total_tool_calls,
                    &mut text_only_nudges,
                    iteration,
                    &cumulative_tokens,
                ) {
                    EmptyToolCallsOutcome::Finish => {
                        return Ok(InferenceResponse {
                            tokens_used: cumulative_tokens,
                            ..response
                        });
                    }
                    EmptyToolCallsOutcome::Nudged => {
                        last_response = Some(response);
                        continue;
                    }
                }
            }

            // Execute tool calls
            let tool_calls_snapshot = response.tool_calls.clone();
            total_tool_calls += tool_calls_snapshot.len();

            // ── Taint gate ──────────────────────────────────────────────────
            // When taint tracking is active, check each outbound call against
            // the current context taint BEFORE executing it. Blocked calls are
            // resolved interactively (allow-once / always-allow / deny) via the
            // injected prompt, or denied outright when no prompt is available.
            let (calls_to_execute, mut denied_results) = self
                .taint_gate_partition(entity, &tool_calls_snapshot)
                .await;
            let mut tool_results = tool_executor(calls_to_execute).await;
            tool_results.append(&mut denied_results);

            self.loop_apply_tool_results(
                entity,
                &response,
                &tool_calls_snapshot,
                tool_results,
                post_tool_sync.as_deref_mut(),
                &mut repetition_detector,
                iteration,
            );

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

            // After adding tool results and running eviction, sync entity->shared
            // so context tools and file tracking see the latest state on the
            // next iteration.
            self.loop_post_sync(entity, post_tool_sync.as_deref_mut());

            last_response = Some(response);
        }

        tracing::warn!(max_iterations, "Inference loop hit max iterations");
        last_response
            .map(|r| InferenceResponse {
                tokens_used: cumulative_tokens,
                ..r
            })
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
            system: vec![],
            messages: vec![
                Message {
                    role: "system".to_string(),
                    content: system_prompt.into(),
                    cache_breakpoint: false,
                },
                Message {
                    role: "user".to_string(),
                    content: user_prompt.into(),
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

    /// Regression: tool-result truncation must never panic on multi-byte UTF-8.
    ///
    /// A `read_files` batch concatenates many UTF-8 files; when the result is
    /// large enough to hit the truncation branches, byte-indexed truncation
    /// (`String::truncate` / `&s[..n]`) panicked whenever the cut landed inside
    /// a multi-byte char (em-dash, smart quote, accent, emoji). That panic ran
    /// in a spawned tool-execution task whose `JoinHandle` is never awaited, so
    /// tokio swallowed it and the run hung forever. `truncate_on_char_boundary`
    /// cuts on char boundaries, so every budget below produces valid UTF-8.
    #[test]
    fn test_truncate_on_char_boundary_never_panics_on_multibyte() {
        // Mixed-width content: em-dash (3 bytes), smart quotes (3), accent (2),
        // emoji (4). Any byte-index cut is very likely to split a char.
        let text = "café — “quote” 🚀 ".repeat(64);
        assert!(
            text.len() > text.chars().count(),
            "must contain multi-byte chars"
        );

        // Sweep every char budget from 0 through the full length: exercises the
        // mid-multibyte-char cut points that used to panic.
        for max_chars in 0..=text.chars().count() + 5 {
            let out = truncate_on_char_boundary(&text, max_chars);
            // `out` is a valid `String` by construction; assert the invariants.
            assert_eq!(out.chars().count(), max_chars.min(text.chars().count()));
            assert!(text.starts_with(&out));
        }
    }

    /// Mirrors the exact expressions at both engine truncation call sites with a
    /// byte budget that lands mid-multibyte-char, proving neither panics.
    #[test]
    fn test_truncation_call_sites_are_char_safe() {
        let result_text = "αβγδε— smart “quotes” 🚀".repeat(16);

        // Site A: routing max_result_tokens truncation.
        let max_chars = 25; // deliberately lands inside a multi-byte char by bytes
        let mut a = truncate_on_char_boundary(&result_text, max_chars);
        a.push_str("\n[...truncated]");
        assert!(a.ends_with("[...truncated]"));

        // Site B: region-full fallback truncation.
        let char_budget = 37;
        let prefix = truncate_on_char_boundary(&result_text, char_budget);
        let omitted = result_text.len().saturating_sub(prefix.len());
        let b = format!("{}... [truncated, {} chars omitted]", prefix, omitted);
        assert!(b.contains("chars omitted"));
        assert!(result_text.starts_with(&prefix));
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
                        leviath_core::RegionKind::SlidingWindow {
                            max_items: 50,
                            eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                        },
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
                        leviath_core::RegionKind::SlidingWindow {
                            max_items: 50,
                            eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                        },
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
        assert_eq!(conv.content[0].kind, leviath_core::EntryKind::UserMessage);
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
                        leviath_core::RegionKind::SlidingWindow {
                            max_items: 50,
                            eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                        },
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
                        leviath_core::RegionKind::SlidingWindow {
                            max_items: 50,
                            eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                        },
                        8000,
                    ));
                    window.add_region(leviath_core::Region::new(
                        "custom".to_string(),
                        leviath_core::RegionKind::SlidingWindow {
                            max_items: 10,
                            eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                        },
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
                    leviath_core::RegionKind::SlidingWindow {
                        max_items: 50,
                        eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                    },
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
                    leviath_core::RegionKind::SlidingWindow {
                        max_items: 50,
                        eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                    },
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
                    leviath_core::RegionKind::SlidingWindow {
                        max_items: 50,
                        eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                    },
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
                    leviath_core::RegionKind::SlidingWindow {
                        max_items: 50,
                        eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                    },
                    8000,
                ));
                window.add_region(leviath_core::Region::new(
                    "tool_results".to_string(),
                    leviath_core::RegionKind::SlidingWindow {
                        max_items: 50,
                        eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                    },
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            6000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            2000,
        ));
        // Add a system message for the context window
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
    async fn test_run_inference_loop_shared_drives_via_handle() {
        // The unified shared-handle entry point drives an agent through an
        // `EngineHandle` and returns the same result as the `&mut self` loop.
        let (engine, entity) = make_engine_with_mock();
        let handle: EngineHandle = Arc::new(tokio::sync::RwLock::new(engine));

        let mut exec = |_calls: Vec<leviath_providers::ToolCall>| -> ToolResultsFuture<'static> {
            Box::pin(async { Vec::new() })
        };

        let result = run_inference_loop_shared(
            &handle,
            entity,
            "mock",
            "test-model",
            Vec::new(),
            10,
            None,
            None,
            &mut exec,
            None,
            None,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "mock response");
        // The handle is still usable after the loop (guard was released).
        assert!(handle
            .read()
            .await
            .world()
            .get::<AgentState>(entity)
            .is_some());
    }

    /// A provider whose `infer` blocks on a shared barrier until *both* agents'
    /// inference calls are in flight. If [`run_inference_loop_shared`] held the
    /// engine lock across `provider.infer`, the second agent could never reach
    /// its `infer` (it would block acquiring the lock), the barrier would never
    /// release, and the test would deadlock. Completing proves the lock is
    /// released across the network call — the whole point of the decomposition.
    struct BarrierProvider {
        name: String,
        barrier: Arc<tokio::sync::Barrier>,
        content: String,
    }

    #[async_trait::async_trait]
    impl leviath_providers::Provider for BarrierProvider {
        async fn infer(
            &self,
            _req: InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            self.barrier.wait().await;
            Ok(InferenceResponse {
                content: self.content.clone(),
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
            &self.name
        }
        fn capabilities(&self, _model: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    /// Spawn a minimal agent (state + inbox + cancel token + a small context
    /// window) into an existing engine, for multi-agent tests.
    fn spawn_mock_agent(engine: &mut AgentEngine, agent_id: &str) -> Entity {
        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "system".to_string(),
            leviath_core::RegionKind::Pinned,
            2000,
        ));
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            6000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            2000,
        ));
        let _ = window.add_to_region("system", "You are a helpful assistant.".to_string(), 6);
        engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: agent_id.to_string(),
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
            .id()
    }

    #[tokio::test]
    async fn test_run_inference_loop_shared_two_agents_overlap_network() {
        // Two agents share one engine handle. A barrier forces both
        // `provider.infer` calls to be in flight at once, so this only completes
        // if the shared driver releases the engine lock across the network call.
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock-a".to_string(),
            Arc::new(BarrierProvider {
                name: "mock-a".to_string(),
                barrier: barrier.clone(),
                content: "response-A".to_string(),
            }),
        );
        registry.register(
            "mock-b".to_string(),
            Arc::new(BarrierProvider {
                name: "mock-b".to_string(),
                barrier: barrier.clone(),
                content: "response-B".to_string(),
            }),
        );
        let mut engine = AgentEngine::with_providers(registry);
        let a = spawn_mock_agent(&mut engine, "agent-a");
        let b = spawn_mock_agent(&mut engine, "agent-b");
        let handle: EngineHandle = Arc::new(tokio::sync::RwLock::new(engine));

        let mut exec_a = |_: Vec<leviath_providers::ToolCall>| -> ToolResultsFuture<'static> {
            Box::pin(async { Vec::new() })
        };
        let mut exec_b = |_: Vec<leviath_providers::ToolCall>| -> ToolResultsFuture<'static> {
            Box::pin(async { Vec::new() })
        };

        let fa = run_inference_loop_shared(
            &handle,
            a,
            "mock-a",
            "m",
            Vec::new(),
            5,
            None,
            None,
            &mut exec_a,
            None,
            None,
        );
        let fb = run_inference_loop_shared(
            &handle,
            b,
            "mock-b",
            "m",
            Vec::new(),
            5,
            None,
            None,
            &mut exec_b,
            None,
            None,
        );

        let (ra, rb) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(fa, fb)
        })
        .await
        .expect("must not deadlock — the engine lock must be released across provider.infer");

        // Each agent got its own response, with no cross-contamination.
        assert_eq!(ra.unwrap().content, "response-A");
        assert_eq!(rb.unwrap().content, "response-B");
        // Both agents made progress and advanced in lockstep (the barrier keeps
        // their per-iteration inference calls paired), confirming isolated,
        // interleaved per-entity state with no lost updates.
        let g = handle.read().await;
        let iter_a = g.world().get::<AgentState>(a).unwrap().iteration;
        let iter_b = g.world().get::<AgentState>(b).unwrap().iteration;
        assert!(iter_a >= 1 && iter_b >= 1);
        assert_eq!(iter_a, iter_b);
    }

    #[tokio::test]
    async fn test_run_inference_loop_shared_cancellation() {
        let (engine, entity) = make_engine_with_mock();
        {
            let token = engine.world().get::<CancellationToken>(entity).unwrap();
            token.cancel();
        }
        let handle: EngineHandle = Arc::new(tokio::sync::RwLock::new(engine));
        let mut exec = |_: Vec<leviath_providers::ToolCall>| -> ToolResultsFuture<'static> {
            Box::pin(async { Vec::new() })
        };
        let result = run_inference_loop_shared(
            &handle,
            entity,
            "mock",
            "m",
            Vec::new(),
            10,
            None,
            None,
            &mut exec,
            None,
            None,
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert_eq!(
            handle
                .read()
                .await
                .world()
                .get::<AgentState>(entity)
                .unwrap()
                .status,
            AgentStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn test_run_inference_loop_shared_with_tool_calls_then_completion() {
        // First response requests a tool; second completes. Exercises the
        // tool-execution + result-append critical sections of the shared driver.
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
            default_response(),
        ];
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::with_responses("mock", responses)),
        );
        let mut engine = AgentEngine::with_providers(registry);
        let entity = spawn_mock_agent(&mut engine, "agent-tools");
        let handle: EngineHandle = Arc::new(tokio::sync::RwLock::new(engine));

        let mut exec = |calls: Vec<leviath_providers::ToolCall>| -> ToolResultsFuture<'static> {
            Box::pin(async move {
                calls
                    .into_iter()
                    .map(|c| (c.id, "tool ok".to_string()))
                    .collect()
            })
        };
        let result = run_inference_loop_shared(
            &handle,
            entity,
            "mock",
            "m",
            Vec::new(),
            10,
            None,
            None,
            &mut exec,
            None,
            None,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().content, "mock response");
        // The tool result was appended to the agent's context window.
        let g = handle.read().await;
        let window = g.world().get::<ContextWindow>(entity).unwrap();
        let assembled = window.assemble();
        let text = format!("{:?}", assembled.messages);
        assert!(text.contains("tool ok"), "tool result should be in context");
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            6000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            6000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
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

        let result = engine
            .run_inference_loop_filtered(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                10,
                None,
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            60000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            4000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            2000,
        ));
        window.add_region(leviath_core::Region::new(
            "scratch".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
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

        let result = engine
            .run_inference_loop_filtered(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                10,
                None,
                None,
                &mut |_tool_calls| async {
                    vec![("call_1".to_string(), "tool output".to_string())]
                },
            )
            .await;

        assert!(result.is_ok());

        // Tool results now go to "conversation" as typed entries
        let w = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = w.get_region("conversation").unwrap();
        assert!(conv
            .content
            .iter()
            .any(|e| matches!(&e.kind, leviath_core::EntryKind::ToolResult { .. })));
    }

    #[tokio::test]
    async fn test_run_inference_loop_filtered_non_persist_routing_falls_back_without_scratch() {
        // Tool results now always go to "conversation" regardless of routing
        // config — this test verifies the loop still runs correctly when
        // routing is configured but no scratch region exists.
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            4000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
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

        let result = engine
            .run_inference_loop_filtered(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                10,
                None,
                None,
                &mut |_tool_calls| async {
                    vec![("call_1".to_string(), "tool output".to_string())]
                },
            )
            .await;
        assert!(result.is_ok());

        // Tool results now go to "conversation" as typed entries
        let w = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = w.get_region("conversation").unwrap();
        assert!(conv
            .content
            .iter()
            .any(|e| matches!(&e.kind, leviath_core::EntryKind::ToolResult { .. })));
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            4000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            2000,
        ));
        window.add_region(leviath_core::Region::new(
            "bash_output".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
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

        let result = engine
            .run_inference_loop_filtered(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                10,
                None,
                None,
                &mut |_tool_calls| async {
                    vec![("call_1".to_string(), "file listing".to_string())]
                },
            )
            .await;

        assert!(result.is_ok());

        // Tool results now go to "conversation" as typed entries
        let w = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = w.get_region("conversation").unwrap();
        assert!(conv
            .content
            .iter()
            .any(|e| matches!(&e.kind, leviath_core::EntryKind::ToolResult { .. })));
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            6000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
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

        engine
            .run_inference_loop_filtered(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                10,
                None,
                None,
                &mut |_| async { vec![("call_1".to_string(), "ok".to_string())] },
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

    // ─── Taint gate enforcement in the inference loop ───────────────────────

    struct FixedPrompt(crate::taint::GateResolution);

    #[async_trait::async_trait]
    impl crate::taint::GatePrompt for FixedPrompt {
        async fn resolve(
            &self,
            _decision: &leviath_core::taint::GateDecision,
        ) -> crate::taint::GateResolution {
            self.0
        }
    }

    fn outbound_tool_response(tool: &str) -> InferenceResponse {
        InferenceResponse {
            content: "calling a tool".to_string(),
            tool_calls: vec![leviath_providers::ToolCall {
                id: "call_1".to_string(),
                name: tool.to_string(),
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
        }
    }

    /// Engine with a Private-tainted context and the gate enabled; the model's
    /// first response calls `tool`. `prompt` is the injected resolver (None →
    /// blocked calls auto-deny), `policy` supplies allowlist rules.
    fn tainted_engine_with(
        tool: &str,
        prompt: Option<Box<dyn crate::taint::GatePrompt>>,
        policy: leviath_core::PolicyConfig,
    ) -> (AgentEngine, Entity) {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::with_responses(
                "mock",
                vec![outbound_tool_response(tool), default_response()],
            )),
        );
        let mut engine = AgentEngine::with_providers(registry);
        let entity = engine
            .world_mut()
            .spawn({
                let mut window = ContextWindow::new(1000);
                window.add_region(leviath_core::Region::new(
                    "notes".to_string(),
                    leviath_core::RegionKind::Pinned,
                    500,
                ));
                window.add_region(leviath_core::Region::new(
                    "conversation".to_string(),
                    leviath_core::RegionKind::SlidingWindow {
                        max_items: 50,
                        eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                    },
                    200,
                ));
                window.enable_taint_tracking();
                window
                    .add_tainted_to_region(
                        "notes",
                        "secret data".to_string(),
                        10,
                        leviath_core::TaintLevel::Private,
                    )
                    .unwrap();
                window
            })
            .id();
        let gate = crate::taint::TaintGate::new(leviath_core::SecurityConfig {
            taint_tracking: true,
            ..leviath_core::SecurityConfig::default()
        });
        engine.configure_taint(gate, policy, prompt);
        (engine, entity)
    }

    fn tainted_engine(resolution: crate::taint::GateResolution) -> (AgentEngine, Entity) {
        tainted_engine_with(
            "shell",
            Some(Box::new(FixedPrompt(resolution))),
            leviath_core::PolicyConfig::default(),
        )
    }

    async fn run_and_record_executed(
        engine: &mut AgentEngine,
        entity: Entity,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
        let executed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let ex = executed.clone();
        engine
            .run_inference_loop_filtered(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                5,
                None,
                None,
                &mut |calls: Vec<leviath_providers::ToolCall>| {
                    let ex = ex.clone();
                    async move {
                        let mut names = ex.lock().unwrap();
                        calls
                            .iter()
                            .map(|c| {
                                names.push(c.name.clone());
                                (c.id.clone(), "executed".to_string())
                            })
                            .collect()
                    }
                },
            )
            .await
            .unwrap();
        executed
    }

    fn tool_results_text(engine: &AgentEngine, entity: Entity) -> String {
        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        window
            .get_region("conversation")
            .unwrap()
            .content
            .iter()
            .map(|e| e.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn taint_gate_denies_blocked_outbound_call() {
        let (mut engine, entity) = tainted_engine(crate::taint::GateResolution::Deny);
        let executed = run_and_record_executed(&mut engine, entity).await;
        // shell was NOT executed (gate denied it before the executor ran).
        assert!(!executed.lock().unwrap().contains(&"shell".to_string()));
        // A [blocked] result was substituted into context.
        assert!(tool_results_text(&engine, entity).contains("[blocked]"));
        // A denied event was audited.
        assert!(engine.taint_audit_log().iter().any(|e| !e.allowed));
    }

    #[tokio::test]
    async fn taint_gate_allow_once_executes_blocked_call() {
        let (mut engine, entity) = tainted_engine(crate::taint::GateResolution::AllowOnce);
        let executed = run_and_record_executed(&mut engine, entity).await;
        // shell WAS executed after the user allowed it once.
        assert!(executed.lock().unwrap().contains(&"shell".to_string()));
        assert!(!tool_results_text(&engine, entity).contains("[blocked]"));
        // The allow was audited.
        assert!(engine.taint_audit_log().iter().any(|e| e.allowed
            && matches!(
                e.decision_source,
                leviath_core::taint::GateDecisionSource::UserAllowOnce
            )));
    }

    #[tokio::test]
    async fn taint_gate_always_allow_executes_and_raises_clearance() {
        let (mut engine, entity) = tainted_engine(crate::taint::GateResolution::AlwaysAllow);
        let executed = run_and_record_executed(&mut engine, entity).await;
        // Executed after the user chose "always allow".
        assert!(executed.lock().unwrap().contains(&"shell".to_string()));
        // Audited as an always-allow decision.
        assert!(engine.taint_audit_log().iter().any(|e| e.allowed
            && matches!(
                e.decision_source,
                leviath_core::taint::GateDecisionSource::UserAlwaysAllow
            )));
        // The tool's clearance was raised to Private for the rest of the run.
        let cls = engine
            .taint_gate
            .as_ref()
            .unwrap()
            .tool_classification("shell");
        assert_eq!(cls.clearance, leviath_core::TaintLevel::Private);
    }

    #[tokio::test]
    async fn taint_gate_disabled_executes_everything() {
        // No gate configured → fast path, tool executes, no audit.
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::with_responses(
                "mock",
                vec![outbound_tool_response("shell"), default_response()],
            )),
        );
        let mut engine = AgentEngine::with_providers(registry);
        let entity = engine
            .world_mut()
            .spawn({
                let mut window = ContextWindow::new(1000);
                window.add_region(leviath_core::Region::new(
                    "tool_results".to_string(),
                    leviath_core::RegionKind::SlidingWindow {
                        max_items: 50,
                        eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                    },
                    200,
                ));
                window
            })
            .id();
        let executed = run_and_record_executed(&mut engine, entity).await;
        assert!(executed.lock().unwrap().contains(&"shell".to_string()));
        assert!(engine.taint_audit_log().is_empty());
    }

    #[tokio::test]
    async fn taint_gate_no_prompt_auto_denies() {
        // Gate enabled but no resolver injected → a blocked outbound call is
        // denied outright (covers the `None => Deny` arm of the partition).
        let (mut engine, entity) =
            tainted_engine_with("shell", None, leviath_core::PolicyConfig::default());
        let executed = run_and_record_executed(&mut engine, entity).await;
        assert!(!executed.lock().unwrap().contains(&"shell".to_string()));
        assert!(tool_results_text(&engine, entity).contains("[blocked]"));
        assert!(engine.taint_audit_log().iter().any(|e| !e.allowed));
    }

    #[tokio::test]
    async fn taint_gate_allows_non_outbound_tool() {
        // An inbound tool (read_file) is never gated even with tainted context;
        // it executes without any prompt (covers the non-outbound path).
        let (mut engine, entity) = tainted_engine_with(
            "read_file",
            None, // resolver must never be consulted for a non-outbound tool
            leviath_core::PolicyConfig::default(),
        );
        let executed = run_and_record_executed(&mut engine, entity).await;
        assert!(executed.lock().unwrap().contains(&"read_file".to_string()));
        assert!(!tool_results_text(&engine, entity).contains("[blocked]"));
    }

    #[tokio::test]
    async fn taint_gate_allowlist_rule_permits_blocked_call() {
        // A matching allowlist rule lets an over-clearance outbound call
        // through without prompting (covers the AllowlistRule branch).
        let mut policy = leviath_core::PolicyConfig::default();
        policy.allowlist.push(leviath_core::policy::AllowlistRule {
            tool: "shell".to_string(),
            to: vec![],
            channel: vec![],
            max_sensitivity: leviath_core::TaintLevel::Private,
        });
        let (mut engine, entity) = tainted_engine_with("shell", None, policy);
        let executed = run_and_record_executed(&mut engine, entity).await;
        assert!(executed.lock().unwrap().contains(&"shell".to_string()));
        assert!(engine.taint_audit_log().iter().any(|e| e.allowed
            && matches!(
                e.decision_source,
                leviath_core::taint::GateDecisionSource::AllowlistRule { .. }
            )));
    }

    #[test]
    fn configure_and_clear_taint() {
        let mut engine = AgentEngine::new();
        assert!(engine.taint_audit_log().is_empty());
        let gate = crate::taint::TaintGate::new(leviath_core::SecurityConfig::default());
        engine.configure_taint(gate, leviath_core::PolicyConfig::default(), None);
        engine.clear_taint();
        assert!(engine.taint_audit_log().is_empty());
    }

    #[test]
    fn enable_entity_taint_tracking_turns_on_region_taint() {
        let mut engine = AgentEngine::new();
        let entity = engine
            .world_mut()
            .spawn({
                let mut window = ContextWindow::new(1000);
                window.add_region(leviath_core::Region::new(
                    "notes".to_string(),
                    leviath_core::RegionKind::Pinned,
                    500,
                ));
                window
            })
            .id();
        // No taint tracking yet → overall_taint is None.
        assert!(engine
            .world()
            .get::<ContextWindow>(entity)
            .unwrap()
            .overall_taint()
            .is_none());
        engine.enable_entity_taint_tracking(entity);
        // Now tracking is on → overall_taint reports (Public with no tainted data).
        assert_eq!(
            engine
                .world()
                .get::<ContextWindow>(entity)
                .unwrap()
                .overall_taint(),
            Some(leviath_core::TaintLevel::Public)
        );
        // Idempotent + safe on a missing entity.
        engine.enable_entity_taint_tracking(entity);
    }

    // ─── Repetition detector triggers nudge in inference loop ─────────

    #[tokio::test]
    async fn repetition_detector_injects_nudge_in_inference_loop() {
        // The model calls the same read_file tool 4 times (threshold 3), then
        // completes. The repetition detector should inject a nudge after the 3rd
        // identical call.
        let tool_resp = |id: &str| InferenceResponse {
            content: "reading".to_string(),
            tool_calls: vec![leviath_providers::ToolCall {
                id: id.to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "foo.rs"}),
            }],
            tokens_used: leviath_providers::TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            finish_reason: leviath_providers::FinishReason::ToolCall,
        };

        let responses = vec![
            tool_resp("c1"),
            tool_resp("c2"),
            tool_resp("c3"), // 3rd identical call → detector nudge
            tool_resp("c4"),
            default_response(), // completion
        ];
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::with_responses("mock", responses)),
        );
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(100_000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 200,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            80_000,
        ));
        let _ = window.add_to_region("conversation", "User: hi".to_string(), 2);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "rep-test".to_string(),
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

        let result = with_tracing_async(engine.run_inference_loop_filtered(
            entity,
            "mock",
            "test-model",
            Vec::new(),
            10,
            None,
            None,
            &mut |calls: Vec<leviath_providers::ToolCall>| async move {
                calls
                    .iter()
                    .map(|c| (c.id.clone(), "file contents".to_string()))
                    .collect()
            },
        ))
        .await;
        assert!(result.is_ok());

        // Verify the nudge was injected — the conversation region should contain
        // a UserMessage entry mentioning repetition or a similar warning.
        let w = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = w.get_region("conversation").unwrap();
        let has_nudge = conv.content.iter().any(|e| {
            e.kind == leviath_core::EntryKind::UserMessage
                && (e.content.contains("times")
                    || e.content.contains("read-only")
                    || e.content.contains("read_file"))
        });
        assert!(
            has_nudge,
            "Repetition detector should have injected a nudge"
        );
    }

    // ─── Text-only nudge path ────────────────────────────────────────

    #[tokio::test]
    async fn text_only_nudge_eventually_accepts_after_max_nudges() {
        // Model responds with text only (no tool calls) every time. After
        // MAX_TEXT_ONLY_NUDGES (3) the loop should accept the text response.
        let mut registry = ProviderRegistry::new();
        // All responses are text-only (no tool calls). Need 4 total: 3 nudged + 1 accepted.
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::new("mock")), // always returns default_response (no tools)
        );
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(100_000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 200,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            80_000,
        ));
        let _ = window.add_to_region("conversation", "User: do something".to_string(), 5);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "nudge-test".to_string(),
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

        let result = with_tracing_async(engine.run_inference_loop_filtered(
            entity,
            "mock",
            "test-model",
            Vec::new(),
            20, // plenty of iterations
            None,
            None,
            &mut noop_tool_exec,
        ))
        .await;

        assert!(result.is_ok());
        // The loop should have added nudge messages to conversation
        let w = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = w.get_region("conversation").unwrap();
        let nudge_count = conv
            .content
            .iter()
            .filter(|e| {
                e.kind == leviath_core::EntryKind::UserMessage
                    && e.content.contains("tools available")
            })
            .count();
        assert!(
            nudge_count >= 1,
            "Expected at least one text-only nudge, found {}",
            nudge_count
        );
    }

    // ─── Tool result truncation in tight context window ──────────────

    #[tokio::test]
    async fn tool_result_truncated_when_region_is_full() {
        // Tool returns a huge result but the conversation region has barely
        // any room, so the result must be truncated rather than dropped.
        let responses = vec![
            InferenceResponse {
                content: "calling tool".to_string(),
                tool_calls: vec![leviath_providers::ToolCall {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
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

        // Very tight conversation region — only 200 tokens total
        let mut window = ContextWindow::new(500);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 200,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            200,
        ));
        let _ = window.add_to_region("conversation", "User: hi".to_string(), 2);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "trunc-test".to_string(),
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

        let huge_result = "x".repeat(10_000);
        let result = engine
            .run_inference_loop_filtered(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                5,
                None,
                None,
                &mut |_calls: Vec<leviath_providers::ToolCall>| {
                    let r = huge_result.clone();
                    async move { vec![("call_1".to_string(), r)] }
                },
            )
            .await;
        assert!(result.is_ok());

        // The tool result should be present (truncated or placeholder)
        let w = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = w.get_region("conversation").unwrap();
        let has_tool_result = conv
            .content
            .iter()
            .any(|e| matches!(&e.kind, leviath_core::EntryKind::ToolResult { .. }));
        assert!(
            has_tool_result,
            "Truncated tool result should still be present"
        );
    }

    // ─── Inference loop with repetition config overrides ─────────────

    #[tokio::test]
    async fn inference_loop_inner_respects_disabled_repetition_config() {
        // Pass a config that disables the detector entirely — same-call
        // repetition should NOT inject a nudge.
        let tool_resp = |id: &str| InferenceResponse {
            content: "reading".to_string(),
            tool_calls: vec![leviath_providers::ToolCall {
                id: id.to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "foo.rs"}),
            }],
            tokens_used: leviath_providers::TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            finish_reason: leviath_providers::FinishReason::ToolCall,
        };

        let responses = vec![
            tool_resp("c1"),
            tool_resp("c2"),
            tool_resp("c3"),
            tool_resp("c4"),
            default_response(),
        ];
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::with_responses("mock", responses)),
        );
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(100_000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 200,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            80_000,
        ));
        let _ = window.add_to_region("conversation", "User: hi".to_string(), 2);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "rep-disabled".to_string(),
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

        let rep_config = leviath_core::RepetitionDetectionConfig {
            enabled: Some(false),
            max_repeat_calls: None,
            max_readonly_streak: None,
        };

        let result = with_tracing_async(engine.run_inference_loop_filtered_dyn_inner(
            entity,
            "mock",
            "test-model",
            Vec::new(),
            10,
            None,
            None,
            &mut |calls: Vec<leviath_providers::ToolCall>| {
                Box::pin(async move {
                    calls
                        .iter()
                        .map(|c| (c.id.clone(), "ok".to_string()))
                        .collect()
                })
            },
            Some(&rep_config),
        ))
        .await;
        assert!(result.is_ok());

        // No nudge should have been injected
        let w = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = w.get_region("conversation").unwrap();
        let has_nudge = conv.content.iter().any(|e| {
            e.kind == leviath_core::EntryKind::UserMessage
                && (e.content.contains("times")
                    || e.content.contains("read-only")
                    || e.content.contains("read_file"))
        });
        assert!(
            !has_nudge,
            "Disabled repetition config should not inject nudges"
        );
    }

    // ─── Taint: tool sensitivity tagging in inference loop ───────────

    #[tokio::test]
    async fn taint_gate_tags_tool_results_with_sensitivity() {
        // When taint tracking is active, tool results should be tagged with
        // the tool's configured sensitivity level.
        let (mut engine, entity) = tainted_engine(crate::taint::GateResolution::AllowOnce);
        // Run the loop — "shell" is outbound, prompt allows it once
        let _ = run_and_record_executed(&mut engine, entity).await;

        // Verify the context window's taint level reflects the tool's output
        let w = engine.world().get::<ContextWindow>(entity).unwrap();
        // The "notes" region has Private taint, so overall should be Private
        assert_eq!(w.overall_taint(), Some(leviath_core::TaintLevel::Private));
    }

    // ─── Additional taint wiring coverage ─────────────────────────────────

    #[test]
    fn enable_entity_taint_tracking_on_missing_entity_is_noop() {
        let mut engine = AgentEngine::new();
        // Fabricate an entity that does not exist in the world.
        let fake_entity = engine.world_mut().spawn_empty().id();
        engine.world_mut().despawn(fake_entity);
        // Should not panic when the entity is gone.
        engine.enable_entity_taint_tracking(fake_entity);
    }

    #[test]
    fn configure_taint_sets_gate_and_policy() {
        let mut engine = AgentEngine::new();
        let gate = crate::taint::TaintGate::new(leviath_core::SecurityConfig {
            taint_tracking: true,
            ..leviath_core::SecurityConfig::default()
        });
        let policy = leviath_core::PolicyConfig::default();
        engine.configure_taint(gate, policy, None);
        // Gate is active so audit log slice should be available (empty but not
        // the None-fallback).
        assert!(engine.taint_gate.is_some());
        assert!(engine.taint_audit_log().is_empty());
    }

    #[test]
    fn taint_audit_log_returns_empty_slice_when_no_gate() {
        let engine = AgentEngine::new();
        // No gate configured → returns the static empty fallback.
        assert!(engine.taint_audit_log().is_empty());
    }

    #[test]
    fn clear_taint_removes_gate_and_prompt() {
        let mut engine = AgentEngine::new();
        let gate = crate::taint::TaintGate::new(leviath_core::SecurityConfig {
            taint_tracking: true,
            ..leviath_core::SecurityConfig::default()
        });
        engine.configure_taint(
            gate,
            leviath_core::PolicyConfig::default(),
            Some(Box::new(FixedPrompt(crate::taint::GateResolution::Deny))),
        );
        assert!(engine.taint_gate.is_some());
        assert!(engine.gate_prompt.is_some());
        engine.clear_taint();
        assert!(engine.taint_gate.is_none());
        assert!(engine.gate_prompt.is_none());
    }

    #[test]
    fn gate_decisions_classifies_outbound_tool_as_blocked() {
        // Directly test the synchronous gate_decisions method.
        let (mut engine, entity) = tainted_engine(crate::taint::GateResolution::Deny);
        let calls = vec![leviath_providers::ToolCall {
            id: "call_test".to_string(),
            name: "shell".to_string(),
            arguments: serde_json::json!({}),
        }];
        let policy = leviath_core::PolicyConfig::default();
        let decisions = engine.gate_decisions(entity, "test-agent", &policy, &calls);
        assert_eq!(decisions.len(), 1);
        // shell is outbound and context is Private → should be blocked.
        assert!(decisions[0].1.blocked_levels().is_some());
    }

    #[test]
    fn gate_decisions_classifies_inbound_tool_as_allowed() {
        // An inbound tool (read_file) should pass through without blocking.
        let (mut engine, entity) = tainted_engine_with(
            "read_file",
            Some(Box::new(FixedPrompt(crate::taint::GateResolution::Deny))),
            leviath_core::PolicyConfig::default(),
        );
        let calls = vec![leviath_providers::ToolCall {
            id: "call_read".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({}),
        }];
        let policy = leviath_core::PolicyConfig::default();
        let decisions = engine.gate_decisions(entity, "test-agent", &policy, &calls);
        assert_eq!(decisions.len(), 1);
        // read_file is inbound → not blocked.
        assert!(decisions[0].1.blocked_levels().is_none());
    }

    #[tokio::test]
    async fn test_evict_and_compact_removes_needs_compaction_marker() {
        // After evict_and_compact, the NeedsCompaction component should be removed
        // even when no compaction is actually needed (only temporary eviction).
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
                temp.add_entry("disposable".to_string(), 950).unwrap();
                window.add_region(temp);
                window.current_tokens = 950;
                window
            })
            .id();

        // Manually add a NeedsCompaction marker
        engine
            .world_mut()
            .entity_mut(entity)
            .insert(NeedsCompaction {
                regions: vec!["scratch".to_string()],
            });
        assert!(engine.world().get::<NeedsCompaction>(entity).is_some());

        let cc = leviath_core::CompactionConfig {
            provider: "unused".to_string(),
            model: "test".to_string(),
            max_summary_tokens: 500,
            temperature: 0.3,
            system_prompt: None,
            user_prompt_template: None,
        };

        engine.evict_and_compact(entity, &cc).await.unwrap();

        // NeedsCompaction should have been removed
        assert!(engine.world().get::<NeedsCompaction>(entity).is_none());
    }

    #[tokio::test]
    async fn test_compact_region_stores_summary_in_compact_history() {
        // When a CompactHistory region exists for the source, the summary
        // should be stored there and the source region should be cleared.
        let mut registry = ProviderRegistry::new();
        registry.register(
            "compact-provider".to_string(),
            Arc::new(MockProvider::new("compact-provider")),
        );
        let mut engine = AgentEngine::with_providers(registry);

        let mut window = ContextWindow::new(10000);
        let mut compacting = leviath_core::Region::new(
            "impl".to_string(),
            leviath_core::RegionKind::Compacting {
                threshold_tokens: 100,
            },
            5000,
        );
        compacting
            .add_entry("detailed analysis here".to_string(), 500)
            .unwrap();
        window.add_region(compacting);

        window.add_region(leviath_core::Region::new(
            "impl_history".to_string(),
            leviath_core::RegionKind::CompactHistory {
                source_region: "impl".to_string(),
            },
            5000,
        ));
        window.current_tokens = 500;

        let entity = engine.world_mut().spawn(window).id();

        let cc = leviath_core::CompactionConfig {
            provider: "compact-provider".to_string(),
            model: "test".to_string(),
            max_summary_tokens: 200,
            temperature: 0.3,
            system_prompt: None,
            user_prompt_template: None,
        };

        engine.compact_region(entity, "impl", &cc).await.unwrap();

        let w = engine.world().get::<ContextWindow>(entity).unwrap();
        // Source region should be cleared
        let impl_region = w.get_region("impl").unwrap();
        assert!(impl_region.content.is_empty());
        // History region should have the summary
        let history = w.get_region("impl_history").unwrap();
        assert!(!history.content.is_empty());
    }

    // ─── PostToolSync / with_sync tests ─────────────────────────────────

    #[tokio::test]
    async fn test_with_sync_none_behaves_like_inner() {
        // Passing None for post_tool_sync should work identically to _inner.
        let responses = vec![
            InferenceResponse {
                content: "calling context tool".to_string(),
                tool_calls: vec![leviath_providers::ToolCall {
                    id: "call_1".to_string(),
                    name: "context_write".to_string(),
                    arguments: serde_json::json!({"region": "conversation", "content": "hi"}),
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            6000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            2000,
        ));
        let _ = window.add_to_region("conversation", "User: hi".to_string(), 2);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "sync-none-test".to_string(),
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
            .run_inference_loop_filtered_dyn_with_sync(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                10,
                None,
                None,
                &mut |_tool_calls| {
                    Box::pin(async { vec![("call_1".to_string(), "ok".to_string())] })
                },
                None,
                None, // no sync callback
            )
            .await;

        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp.content, "mock response");
    }

    #[tokio::test]
    async fn test_with_sync_callback_called_for_context_tools() {
        // When a context_* tool is in the response, the sync callback should be
        // invoked twice per iteration: once before adding tool results and once
        // after eviction.
        let sync_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sync_count_clone = sync_count.clone();

        let responses = vec![
            InferenceResponse {
                content: "writing context".to_string(),
                tool_calls: vec![leviath_providers::ToolCall {
                    id: "call_ctx".to_string(),
                    name: "context_write".to_string(),
                    arguments: serde_json::json!({"region": "conversation", "content": "data"}),
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            6000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            2000,
        ));
        let _ = window.add_to_region("conversation", "User: hi".to_string(), 2);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "sync-some-test".to_string(),
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

        let mut sync_cb: Box<PostToolSync> = Box::new(
            move |_world: &mut bevy_ecs::prelude::World, _ent: bevy_ecs::prelude::Entity| {
                sync_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            },
        );

        let result = engine
            .run_inference_loop_filtered_dyn_with_sync(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                10,
                None,
                None,
                &mut |_tool_calls| {
                    Box::pin(async { vec![("call_ctx".to_string(), "ok".to_string())] })
                },
                None,
                Some(&mut *sync_cb),
            )
            .await;

        assert!(result.is_ok());
        // Sync should be called exactly 2 times: before tool results + after eviction
        assert_eq!(sync_count.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_with_sync_called_for_all_tool_batches() {
        // Sync callback fires for ALL tool batches (not just context_* tools)
        // because file tracking also modifies the shared context window.
        let sync_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sync_count_clone = sync_count.clone();

        let responses = vec![
            InferenceResponse {
                content: "running bash".to_string(),
                tool_calls: vec![leviath_providers::ToolCall {
                    id: "call_bash".to_string(),
                    name: "bash".to_string(),
                    arguments: serde_json::json!({"cmd": "echo hi"}),
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            6000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            2000,
        ));
        let _ = window.add_to_region("conversation", "User: hi".to_string(), 2);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "sync-no-ctx-test".to_string(),
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

        let mut sync_cb: Box<PostToolSync> = Box::new(
            move |_world: &mut bevy_ecs::prelude::World, _ent: bevy_ecs::prelude::Entity| {
                sync_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            },
        );

        let result = engine
            .run_inference_loop_filtered_dyn_with_sync(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                10,
                None,
                None,
                &mut |_tool_calls| {
                    Box::pin(async { vec![("call_bash".to_string(), "ok".to_string())] })
                },
                None,
                Some(&mut *sync_cb),
            )
            .await;

        assert!(result.is_ok());
        // Sync fires for every tool batch (pre-results + post-eviction = 2 per batch)
        assert_eq!(sync_count.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_with_sync_alternating_direction() {
        // Verify that the sync callback receives the correct world/entity and
        // can observe context window state changes between calls.
        let directions = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let directions_clone = directions.clone();

        let responses = vec![
            InferenceResponse {
                content: "context tool".to_string(),
                tool_calls: vec![leviath_providers::ToolCall {
                    id: "call_ctx".to_string(),
                    name: "context_read".to_string(),
                    arguments: serde_json::json!({"region": "conversation"}),
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
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            6000,
        ));
        window.add_region(leviath_core::Region::new(
            "tool_results".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            2000,
        ));
        let _ = window.add_to_region("conversation", "User: hi".to_string(), 2);

        let entity = engine
            .world_mut()
            .spawn((
                AgentState {
                    agent_id: "sync-dir-test".to_string(),
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

        let mut sync_cb: Box<PostToolSync> = Box::new(
            move |world: &mut bevy_ecs::prelude::World, ent: bevy_ecs::prelude::Entity| {
                // Record that we can access the entity's context window
                let has_cw = world.get::<ContextWindow>(ent).is_some();
                directions_clone
                    .lock()
                    .unwrap()
                    .push(format!("sync:has_cw={}", has_cw));
            },
        );

        let result = engine
            .run_inference_loop_filtered_dyn_with_sync(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                10,
                None,
                None,
                &mut |_tool_calls| {
                    Box::pin(async { vec![("call_ctx".to_string(), "ok".to_string())] })
                },
                None,
                Some(&mut *sync_cb),
            )
            .await;

        assert!(result.is_ok());
        let dirs = directions.lock().unwrap();
        // Both sync calls should have access to the entity's ContextWindow
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0], "sync:has_cw=true");
        assert_eq!(dirs[1], "sync:has_cw=true");
    }

    // ── Tool result routing tests ─────────────────────────────────────────

    /// Helper: build a single-tool-call response followed by a final response.
    fn tool_call_then_done(tool_id: &str, tool_name: &str) -> Vec<InferenceResponse> {
        vec![
            InferenceResponse {
                content: format!("calling {}", tool_name),
                tool_calls: vec![leviath_providers::ToolCall {
                    id: tool_id.to_string(),
                    name: tool_name.to_string(),
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
        ]
    }

    /// Helper: create engine with mock provider from responses.
    fn engine_with_mock(responses: Vec<InferenceResponse>) -> AgentEngine {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            Arc::new(MockProvider::with_responses("mock", responses)),
        );
        AgentEngine::with_providers(registry)
    }

    /// Helper: spawn an entity with the given context window and optional routing component.
    fn spawn_routing_entity(
        engine: &mut AgentEngine,
        agent_id: &str,
        window: ContextWindow,
        routing: Option<leviath_core::ToolResultRouting>,
    ) -> Entity {
        let mut builder = engine.world_mut().spawn((
            AgentState {
                agent_id: agent_id.to_string(),
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
        ));
        if let Some(r) = routing {
            builder.insert(crate::ToolResultRoutingComponent { routing: r });
        }
        builder.id()
    }

    /// Helper: run the inference loop with a simple tool executor that returns
    /// the given result for the given tool call id.
    async fn run_with_result(
        engine: &mut AgentEngine,
        entity: Entity,
        tool_call_id: &str,
        result_text: &str,
    ) -> Result<InferenceResponse, ProviderError> {
        let id = tool_call_id.to_string();
        let text = result_text.to_string();
        engine
            .run_inference_loop_filtered_dyn_with_sync(
                entity,
                "mock",
                "test-model",
                Vec::new(),
                10,
                None,
                None,
                &mut |_tool_calls| {
                    let id = id.clone();
                    let text = text.clone();
                    Box::pin(async move { vec![(id, text)] })
                },
                None,
                None,
            )
            .await
    }

    /// Helper: create a basic context window with a "conversation" region.
    fn basic_window() -> ContextWindow {
        let mut window = ContextWindow::new(10000);
        window.add_region(leviath_core::Region::new(
            "conversation".to_string(),
            leviath_core::RegionKind::SlidingWindow {
                max_items: 50,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            8000,
        ));
        let _ = window.add_to_region("conversation", "User: hi".to_string(), 2);
        window
    }

    #[tokio::test]
    async fn test_tool_result_routes_to_conversation_when_no_routing_component() {
        with_tracing_async(async {
            let responses = tool_call_then_done("call_1", "bash");
            let mut engine = engine_with_mock(responses);

            let window = basic_window();
            let entity = spawn_routing_entity(&mut engine, "no-routing", window, None);

            let result = run_with_result(&mut engine, entity, "call_1", "hello world").await;
            assert!(result.is_ok());

            let window = engine.world().get::<ContextWindow>(entity).unwrap();
            let conv = window.get_region("conversation").unwrap();
            // Should contain: initial user message, assistant turn, tool result
            let tool_results: Vec<_> = conv
                .content
                .iter()
                .filter(|e| matches!(e.kind, leviath_core::EntryKind::ToolResult { .. }))
                .collect();
            assert_eq!(
                tool_results.len(),
                1,
                "tool result should be in conversation region"
            );
            assert!(tool_results[0].content.contains("hello world"));
        })
        .await;
    }

    #[tokio::test]
    async fn test_tool_result_routes_to_default_region_from_routing_component() {
        with_tracing_async(async {
            let responses = tool_call_then_done("call_1", "bash");
            let mut engine = engine_with_mock(responses);

            let mut window = basic_window();
            window.add_region(leviath_core::Region::new(
                "tool_output".to_string(),
                leviath_core::RegionKind::SlidingWindow {
                    max_items: 50,
                    eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                },
                4000,
            ));

            let routing = leviath_core::ToolResultRouting {
                default_region: "tool_output".to_string(),
                tool_overrides: HashMap::new(),
                persist: true,
                max_result_tokens: None,
            };
            let entity = spawn_routing_entity(&mut engine, "default-region", window, Some(routing));

            let result = run_with_result(&mut engine, entity, "call_1", "routed result").await;
            assert!(result.is_ok());

            let window = engine.world().get::<ContextWindow>(entity).unwrap();

            // Tool result should be in tool_output, NOT in conversation
            let tool_output = window.get_region("tool_output").unwrap();
            let results_in_output: Vec<_> = tool_output
                .content
                .iter()
                .filter(|e| matches!(e.kind, leviath_core::EntryKind::ToolResult { .. }))
                .collect();
            assert_eq!(
                results_in_output.len(),
                1,
                "tool result should be in tool_output region"
            );
            assert!(results_in_output[0].content.contains("routed result"));

            // Conversation should have the assistant turn but NOT the tool result
            let conv = window.get_region("conversation").unwrap();
            let results_in_conv: Vec<_> = conv
                .content
                .iter()
                .filter(|e| matches!(e.kind, leviath_core::EntryKind::ToolResult { .. }))
                .collect();
            assert_eq!(
                results_in_conv.len(),
                0,
                "tool result should NOT be in conversation"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn test_tool_result_per_tool_override_routes_to_different_region() {
        with_tracing_async(async {
            // Two tool calls: "bash" should go to "bash_output", "read_file" to default "tool_output"
            let responses = vec![
                InferenceResponse {
                    content: "calling tools".to_string(),
                    tool_calls: vec![
                        leviath_providers::ToolCall {
                            id: "call_bash".to_string(),
                            name: "bash".to_string(),
                            arguments: serde_json::json!({}),
                        },
                        leviath_providers::ToolCall {
                            id: "call_read".to_string(),
                            name: "read_file".to_string(),
                            arguments: serde_json::json!({}),
                        },
                    ],
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
            let mut engine = engine_with_mock(responses);

            let mut window = basic_window();
            window.add_region(leviath_core::Region::new(
                "tool_output".to_string(),
                leviath_core::RegionKind::SlidingWindow {
                    max_items: 50,
                    eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                },
                4000,
            ));
            window.add_region(leviath_core::Region::new(
                "bash_output".to_string(),
                leviath_core::RegionKind::SlidingWindow {
                    max_items: 50,
                    eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                },
                4000,
            ));

            let mut overrides = HashMap::new();
            overrides.insert("bash".to_string(), "bash_output".to_string());

            let routing = leviath_core::ToolResultRouting {
                default_region: "tool_output".to_string(),
                tool_overrides: overrides,
                persist: true,
                max_result_tokens: None,
            };
            let entity = spawn_routing_entity(&mut engine, "override-test", window, Some(routing));

            let result = engine
                .run_inference_loop_filtered_dyn_with_sync(
                    entity,
                    "mock",
                    "test-model",
                    Vec::new(),
                    10,
                    None,
                    None,
                    &mut |_tool_calls| {
                        Box::pin(async {
                            vec![
                                ("call_bash".to_string(), "bash output".to_string()),
                                ("call_read".to_string(), "file content".to_string()),
                            ]
                        })
                    },
                    None,
                    None,
                )
                .await;
            assert!(result.is_ok());

            let window = engine.world().get::<ContextWindow>(entity).unwrap();

            // bash tool result should be in bash_output (per override)
            let bash_region = window.get_region("bash_output").unwrap();
            let bash_results: Vec<_> = bash_region
                .content
                .iter()
                .filter(|e| matches!(e.kind, leviath_core::EntryKind::ToolResult { .. }))
                .collect();
            assert_eq!(
                bash_results.len(),
                1,
                "bash result should be in bash_output"
            );
            assert!(bash_results[0].content.contains("bash output"));

            // read_file tool result should be in tool_output (default)
            let tool_region = window.get_region("tool_output").unwrap();
            let read_results: Vec<_> = tool_region
                .content
                .iter()
                .filter(|e| matches!(e.kind, leviath_core::EntryKind::ToolResult { .. }))
                .collect();
            assert_eq!(
                read_results.len(),
                1,
                "read_file result should be in tool_output"
            );
            assert!(read_results[0].content.contains("file content"));
        })
        .await;
    }

    #[tokio::test]
    async fn test_tool_result_max_result_tokens_truncation() {
        with_tracing_async(async {
            let responses = tool_call_then_done("call_1", "bash");
            let mut engine = engine_with_mock(responses);

            let window = basic_window();

            // max_result_tokens=5 means max_chars=20 (5*4)
            let routing = leviath_core::ToolResultRouting {
                default_region: "conversation".to_string(),
                tool_overrides: HashMap::new(),
                persist: true,
                max_result_tokens: Some(5),
            };
            let entity = spawn_routing_entity(&mut engine, "truncate-test", window, Some(routing));

            // Result text is 40 chars, limit is 20 chars
            let long_result = "a]".repeat(20); // 40 chars
            let result = run_with_result(&mut engine, entity, "call_1", &long_result).await;
            assert!(result.is_ok());

            let window = engine.world().get::<ContextWindow>(entity).unwrap();
            let conv = window.get_region("conversation").unwrap();
            let tool_results: Vec<_> = conv
                .content
                .iter()
                .filter(|e| matches!(e.kind, leviath_core::EntryKind::ToolResult { .. }))
                .collect();
            assert_eq!(tool_results.len(), 1);
            // Should be truncated to 20 chars + "[...truncated]" suffix
            assert!(
                tool_results[0].content.contains("[...truncated]"),
                "result should contain truncation marker, got: {}",
                tool_results[0].content
            );
            assert!(
                tool_results[0].content.len() < long_result.len(),
                "truncated result ({}) should be shorter than original ({})",
                tool_results[0].content.len(),
                long_result.len()
            );
        })
        .await;
    }

    /// Regression (through the real engine path) for the `read_files` hang:
    /// max_result_tokens truncation of a multi-byte UTF-8 tool result.
    ///
    /// max_result_tokens=5 → max_chars=20. Byte 20 of the content below lands
    /// inside a 3-byte "—", so the old `result_text.truncate(20)` panicked; that
    /// panic in the tool-result path is what stalled `read_files` runs. With the
    /// char-safe truncation this completes and produces valid UTF-8.
    #[tokio::test]
    async fn test_tool_result_truncation_multibyte_utf8_does_not_panic() {
        with_tracing_async(async {
            let responses = tool_call_then_done("call_1", "bash");
            let mut engine = engine_with_mock(responses);
            let window = basic_window();

            let routing = leviath_core::ToolResultRouting {
                default_region: "conversation".to_string(),
                tool_overrides: HashMap::new(),
                persist: true,
                max_result_tokens: Some(5), // max_chars = 20
            };
            let entity =
                spawn_routing_entity(&mut engine, "truncate-multibyte", window, Some(routing));

            // 7 chars / 13 bytes per unit; byte 20 is mid-"—" (a non-boundary).
            let long_result = "café—🚀 ".repeat(8);
            let result = run_with_result(&mut engine, entity, "call_1", &long_result).await;
            assert!(result.is_ok(), "engine loop must not panic on multibyte");

            let window = engine.world().get::<ContextWindow>(entity).unwrap();
            let conv = window.get_region("conversation").unwrap();
            let tool_results: Vec<_> = conv
                .content
                .iter()
                .filter(|e| matches!(e.kind, leviath_core::EntryKind::ToolResult { .. }))
                .collect();
            assert_eq!(tool_results.len(), 1);
            assert!(tool_results[0].content.contains("[...truncated]"));
            // Content is a valid `String` by construction; confirm it truncated.
            assert!(tool_results[0].content.len() < long_result.len());
        })
        .await;
    }

    #[tokio::test]
    async fn test_tool_result_persist_false_routes_to_scratch_when_available() {
        with_tracing_async(async {
            let responses = tool_call_then_done("call_1", "bash");
            let mut engine = engine_with_mock(responses);

            let mut window = basic_window();
            window.add_region(leviath_core::Region::new(
                "scratch".to_string(),
                leviath_core::RegionKind::SlidingWindow {
                    max_items: 50,
                    eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                },
                4000,
            ));

            let routing = leviath_core::ToolResultRouting {
                default_region: "conversation".to_string(),
                tool_overrides: HashMap::new(),
                persist: false,
                max_result_tokens: None,
            };
            let entity = spawn_routing_entity(&mut engine, "persist-false", window, Some(routing));

            let result = run_with_result(&mut engine, entity, "call_1", "ephemeral data").await;
            assert!(result.is_ok());

            let window = engine.world().get::<ContextWindow>(entity).unwrap();

            // Tool result should be in scratch, NOT in conversation
            let scratch = window.get_region("scratch").unwrap();
            let scratch_results: Vec<_> = scratch
                .content
                .iter()
                .filter(|e| matches!(e.kind, leviath_core::EntryKind::ToolResult { .. }))
                .collect();
            assert_eq!(
                scratch_results.len(),
                1,
                "tool result should be in scratch region"
            );
            assert!(scratch_results[0].content.contains("ephemeral data"));

            let conv = window.get_region("conversation").unwrap();
            let conv_results: Vec<_> = conv
                .content
                .iter()
                .filter(|e| matches!(e.kind, leviath_core::EntryKind::ToolResult { .. }))
                .collect();
            assert_eq!(
                conv_results.len(),
                0,
                "tool result should NOT be in conversation"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn test_tool_result_persist_false_falls_back_when_no_scratch_region() {
        with_tracing_async(async {
            let responses = tool_call_then_done("call_1", "bash");
            let mut engine = engine_with_mock(responses);

            let mut window = basic_window();
            // Add target region but NO scratch region
            window.add_region(leviath_core::Region::new(
                "tool_output".to_string(),
                leviath_core::RegionKind::SlidingWindow {
                    max_items: 50,
                    eviction_strategy: leviath_core::EvictionStrategy::PerItem,
                },
                4000,
            ));

            let routing = leviath_core::ToolResultRouting {
                default_region: "tool_output".to_string(),
                tool_overrides: HashMap::new(),
                persist: false,
                max_result_tokens: None,
            };
            let entity = spawn_routing_entity(&mut engine, "no-scratch", window, Some(routing));

            let result = run_with_result(&mut engine, entity, "call_1", "fallback data").await;
            assert!(result.is_ok());

            let window = engine.world().get::<ContextWindow>(entity).unwrap();

            // No scratch region exists, so should fall back to default_region (tool_output)
            let tool_output = window.get_region("tool_output").unwrap();
            let results: Vec<_> = tool_output
                .content
                .iter()
                .filter(|e| matches!(e.kind, leviath_core::EntryKind::ToolResult { .. }))
                .collect();
            assert_eq!(
                results.len(),
                1,
                "tool result should fall back to tool_output"
            );
            assert!(results[0].content.contains("fallback data"));

            // Conversation should NOT have the tool result
            let conv = window.get_region("conversation").unwrap();
            let conv_results: Vec<_> = conv
                .content
                .iter()
                .filter(|e| matches!(e.kind, leviath_core::EntryKind::ToolResult { .. }))
                .collect();
            assert_eq!(
                conv_results.len(),
                0,
                "tool result should NOT be in conversation"
            );
        })
        .await;
    }

    #[test]
    fn drain_pending_messages_noop_when_entity_has_no_inbox() {
        let mut engine = AgentEngine::new();
        // Spawn an entity that has an AgentState but NO MessageInbox component.
        let entity = engine
            .world_mut()
            .spawn(AgentState {
                agent_id: "no-inbox".to_string(),
                current_stage: "main".to_string(),
                iteration: 0,
                status: AgentStatus::Active,
                spawned_children_ids: Vec::new(),
                pending_wait: None,
                accepts_messages: true,
            })
            .id();
        // get_mut::<MessageInbox> returns None → the if-let body is skipped and
        // the call is a no-op that must not panic.
        engine.drain_pending_messages(entity);
    }
}
