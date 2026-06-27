//! Agent execution engine using bevy_ecs.

use bevy_ecs::prelude::*;
use leviath_providers::{
    InferenceRequest, InferenceResponse, Message, Provider, ProviderError, Tool,
};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::info;

use crate::components::{AgentState, ContextWindow, InferenceResult};
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

        Self {
            world,
            schedule,
            provider_registry: ProviderRegistry::new(),
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

        Self {
            world,
            schedule,
            provider_registry,
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

    /// Run inference for a specific agent.
    ///
    /// This is the core inference loop:
    /// 1. Build InferenceRequest from the agent's ContextWindow
    /// 2. Look up the correct provider based on model config
    /// 3. Call provider.infer()
    /// 4. Parse tool calls from the response
    /// 5. Add response to conversation region
    /// 6. Return the response for the caller to handle tool execution
    pub async fn run_inference(
        &mut self,
        entity: Entity,
        provider_name: &str,
        model: &str,
        tools: Vec<Tool>,
    ) -> std::result::Result<InferenceResponse, ProviderError> {
        let provider = self
            .provider_registry
            .get(provider_name)
            .ok_or_else(|| {
                ProviderError::Other(format!("Provider '{}' not registered", provider_name))
            })?
            .clone();

        // Build the prompt from the context window
        let (prompt, max_tokens) = {
            let window = self
                .world
                .get::<ContextWindow>(entity)
                .ok_or_else(|| ProviderError::Other("Entity has no ContextWindow".to_string()))?;

            let prompt = window.assemble_prompt();
            let remaining = window.max_tokens.saturating_sub(window.current_tokens);
            let max_tokens = remaining.min(4096); // Cap at 4096 for response
            (prompt, max_tokens)
        };

        let request = InferenceRequest {
            messages: vec![Message {
                role: "user".to_string(),
                content: prompt,
            }],
            model: model.to_string(),
            max_tokens,
            temperature: 0.7,
            tools,
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
        let mut last_response = None;

        for iteration in 0..max_iterations {
            tracing::debug!(iteration, "Inference loop iteration");

            let response = self
                .run_inference(entity, provider_name, model, tools.clone())
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
            let tool_results = tool_executor(response.tool_calls.clone()).await;

            // Add tool results to context window
            if let Some(mut window) = self.world.get_mut::<ContextWindow>(entity) {
                // Add assistant response
                let response_tokens = response.content.len() / 4;
                let _ = window.add_to_region(
                    "conversation",
                    format!("Assistant: {}", response.content),
                    response_tokens,
                );

                // Add tool results
                for (tool_call_id, result) in &tool_results {
                    let result_tokens = result.len() / 4;
                    let _ = window.add_to_region(
                        "tool_results",
                        format!("[Tool {}]: {}", tool_call_id, result),
                        result_tokens,
                    );
                }
            }

            last_response = Some(response);
        }

        tracing::warn!(max_iterations, "Inference loop hit max iterations");
        last_response
            .ok_or_else(|| ProviderError::Other("No response generated".to_string()))
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
}
