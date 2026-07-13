//! Shared helper functions: title generation, context window setup, snapshots.

use leviath_core::{Blueprint, ContextLayout, EvictionStrategy, Region, RegionKind};
use leviath_runtime::{AgentEngine, ContextWindow};

use crate::config::Config;
use crate::runstate;

/// Return the cheapest fast model for a given provider, used for title generation.
pub fn default_title_model(provider: &str) -> &'static str {
    match provider {
        "anthropic" | "claude-code" => "claude-haiku-4-5-20251001",
        "openai" => "gpt-5.4-mini",
        "google" => "gemini-3.5-flash",
        "openrouter" => "anthropic/claude-haiku-4-5",
        // For Ollama and unknown providers, fall through to the caller's
        // logic which will prefer config.default_model or the run model.
        _ => "",
    }
}

/// Resolve the user's configured default model into a `(provider, model)` pair.
///
/// `default_model` may itself name a provider via the `provider/model` syntax;
/// otherwise the standalone `default_provider` is paired with it. Returns
/// `None` when no default model is configured. Used as the executor's
/// last-resort fallback when a stage's listed providers are all unavailable.
pub fn resolve_user_default_model(config: &Config) -> Option<(String, String)> {
    config.default_model.as_ref().map(|m| {
        if let Some((provider, model)) = m.split_once('/') {
            (provider.to_string(), model.to_string())
        } else {
            (config.default_provider.clone(), m.clone())
        }
    })
}

/// Attempt to generate a short title from the task prompt using a cheap model.
///
/// Best-effort: any failure is logged and silently ignored — a missing title
/// must never prevent the run from starting.  Token usage from this call is
/// intentionally excluded from the run's prompt/completion accumulators.
pub async fn generate_title(
    task: &str,
    config: &Config,
    registry: &leviath_runtime::ProviderRegistry,
    fallback_model: Option<&str>,
) -> Option<String> {
    let provider_name = config
        .title
        .provider
        .as_deref()
        .unwrap_or(&config.default_provider);

    let provider = registry.get(provider_name)?;

    let model = config
        .title
        .model
        .as_deref()
        .map(|s| s.to_string())
        .or_else(|| {
            let m = default_title_model(provider_name);
            if m.is_empty() {
                fallback_model.map(|s| s.to_string())
            } else {
                Some(m.to_string())
            }
        })?;

    let request = leviath_providers::InferenceRequest {
        messages: vec![leviath_providers::Message {
            role: "user".to_string(),
            content: task.to_string().into(),
            cache_breakpoint: false,
        }],
        model,
        max_tokens: 20,
        temperature: 0.0,
        tools: vec![],
        system: vec![leviath_providers::SystemBlock {
            text: "Write a terse 3-6 word title summarising the task. \
                   No quotes, no punctuation at the end, no markdown."
                .to_string(),
            cache_hint: leviath_core::CacheHint::Never,
        }],
        extra: serde_json::Value::Null,
    };

    match provider.infer(request).await {
        Ok(resp) => {
            let trimmed = resp.content.trim();
            // If the model wrapped its answer in a fenced code block (``` or
            // ```lang), the opening fence line is just a delimiter/language
            // tag — unwrap it first so it isn't mistaken for the title itself
            // (e.g. ```python\nWeb Page Downloader\n``` must not become "python").
            let content = if trimmed.starts_with("```") {
                let rest: String = trimmed.lines().skip(1).collect::<Vec<_>>().join("\n");
                rest.trim().trim_end_matches("```").trim().to_string()
            } else {
                trimmed.to_string()
            };
            let raw = content.lines().next()?.trim().to_string();
            // Strip leading # heading markers, backtick code formatting, surrounding quotes
            let title = raw
                .trim_start_matches('#')
                .trim()
                .trim_start_matches('`')
                .trim_end_matches('`')
                .trim_start_matches('"')
                .trim_end_matches('"')
                .trim_start_matches('\'')
                .trim_end_matches('\'')
                .trim()
                .to_string();
            if title.is_empty() {
                None
            } else {
                Some(title)
            }
        }
        Err(e) => {
            println!("Warning: title generation failed ({})", e);
            None
        }
    }
}

/// Initialize context window regions on an entity from the blueprint.
pub fn initialize_context_window(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    blueprint: &Blueprint,
    task: &str,
) {
    if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
        for region_def in &blueprint.context_layout.regions {
            let region = Region::new(
                region_def.name.clone(),
                region_def.kind.clone(),
                region_def.max_tokens,
            );
            window.add_region(region);
        }

        if window.get_region("tool_results").is_none() {
            let tool_region = Region::new("tool_results".to_string(), RegionKind::Temporary, 5000);
            window.add_region(tool_region);
        }

        if window.get_region("conversation").is_none() {
            let conv_region = Region::new(
                "conversation".to_string(),
                RegionKind::SlidingWindow {
                    max_items: 50,
                    eviction_strategy: EvictionStrategy::PerItem,
                },
                10000,
            );
            window.add_region(conv_region);
        }

        let system_region_name = blueprint
            .context_layout
            .regions
            .iter()
            .find(|r| matches!(r.kind, RegionKind::Pinned))
            .map(|r| r.name.clone());

        if let Some(region_name) = system_region_name {
            let task_tokens = task.len() / 4 + 1;
            let _ = window.add_to_region(&region_name, task.to_string(), task_tokens);
        }
    }
}

/// Swap context layout to a stage-specific layout (preserving existing content where possible).
pub fn swap_context_layout(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    layout: &ContextLayout,
) {
    if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
        let mut new_regions = Vec::new();
        for region_def in &layout.regions {
            let mut new_region = Region::new(
                region_def.name.clone(),
                region_def.kind.clone(),
                region_def.max_tokens,
            );

            if let Some(existing) = window.get_region(&region_def.name) {
                for entry in &existing.content {
                    let _ = new_region.add_entry(entry.content.clone(), entry.tokens);
                }
            }

            new_regions.push(new_region);
        }

        window.regions = new_regions;
        window.current_tokens = window.calculate_tokens();
    }
}

/// Snapshot the current context window to `context.json` for the background dashboard.
/// No-op when running in foreground mode (run_id is None).
pub fn write_context_snapshot_if_bg(
    engine: &AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    stage_name: &str,
    run_id: &Option<String>,
) {
    let Some(ref rid) = run_id else { return };
    let Some(snap) = build_context_snapshot(engine, entity, stage_name) else {
        return;
    };
    let _ = runstate::write_context_snapshot(rid, &snap);
}

/// Build a ContextSnapshot from the current engine state (reused by legacy and per-stage writes).
pub fn build_context_snapshot(
    engine: &AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    stage_name: &str,
) -> Option<runstate::ContextSnapshot> {
    let window = engine.world().get::<ContextWindow>(entity)?;
    let regions = window
        .regions
        .iter()
        .map(|r| {
            let entries = r
                .content
                .iter()
                .map(|e| runstate::RegionEntrySnapshot {
                    content: e.content.clone(),
                    tokens: e.tokens,
                    metadata: e.metadata.clone(),
                    key: e.key.clone(),
                })
                .collect();
            runstate::RegionSnapshot {
                name: r.name.clone(),
                kind: match &r.kind {
                    RegionKind::Pinned => "pinned",
                    RegionKind::Temporary => "temporary",
                    RegionKind::Clearable => "clearable",
                    RegionKind::SlidingWindow { .. } => "sliding",
                    RegionKind::Compacting { .. } => "compacting",
                    RegionKind::CompactHistory { .. } => "history",
                    RegionKind::HashMap { .. } => "hashmap",
                }
                .to_string(),
                current_tokens: r.current_tokens,
                max_tokens: r.max_tokens,
                entries,
            }
        })
        .collect();
    Some(runstate::ContextSnapshot {
        stage_name: stage_name.to_string(),
        total_tokens: window.current_tokens,
        max_tokens: window.max_tokens,
        regions,
    })
}

/// Write a line to the per-stage readable output (agent responses).
pub fn record_stage_output(run_id: &str, idx: usize, text: &str) {
    runstate::append_stage_output(run_id, idx, text);
}

/// Write a line to the per-stage operational/tool log.
pub fn record_stage_log(run_id: &str, idx: usize, text: &str) {
    runstate::append_stage_log(run_id, idx, text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use leviath_core::blueprint::ModelConfig;
    use leviath_core::layout::RegionDefinition;
    use leviath_providers::{
        FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, Provider,
        ProviderError, TokenUsage,
    };
    use leviath_runtime::{AgentPool, ProviderRegistry};

    #[test]
    fn resolve_user_default_model_none_when_unset() {
        let config = Config {
            default_model: None,
            ..Config::default()
        };
        assert_eq!(resolve_user_default_model(&config), None);
    }

    #[test]
    fn resolve_user_default_model_uses_default_provider_without_slash() {
        let config = Config {
            default_provider: "openai".to_string(),
            default_model: Some("gpt-4o".to_string()),
            ..Config::default()
        };
        assert_eq!(
            resolve_user_default_model(&config),
            Some(("openai".to_string(), "gpt-4o".to_string()))
        );
    }

    #[test]
    fn resolve_user_default_model_parses_provider_slash_syntax() {
        let config = Config {
            default_provider: "openai".to_string(),
            default_model: Some("anthropic/claude-sonnet-4-6".to_string()),
            ..Config::default()
        };
        // The `provider/model` syntax overrides default_provider.
        assert_eq!(
            resolve_user_default_model(&config),
            Some(("anthropic".to_string(), "claude-sonnet-4-6".to_string()))
        );
    }

    /// A mock provider returning a fixed canned response, used to exercise
    /// generate_title()'s response-parsing logic without a real network call.
    struct CannedTitleProvider {
        content: String,
    }

    #[async_trait]
    impl Provider for CannedTitleProvider {
        async fn infer(
            &self,
            _request: InferenceRequest,
        ) -> Result<InferenceResponse, ProviderError> {
            Ok(InferenceResponse {
                content: self.content.clone(),
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
            "mock"
        }

        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }

    fn make_title_registry(content: &str) -> ProviderRegistry {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            std::sync::Arc::new(CannedTitleProvider {
                content: content.to_string(),
            }),
        );
        registry
    }

    fn make_title_config() -> Config {
        Config {
            title: crate::config::TitleConfig {
                enabled: true,
                provider: Some("mock".to_string()),
                model: Some("mock-model".to_string()),
            },
            ..Config::default()
        }
    }

    fn make_blueprint_with_regions(regions: Vec<RegionDefinition>) -> leviath_core::Blueprint {
        let total = regions.iter().map(|r| r.max_tokens).sum();
        let layout = ContextLayout::new(regions, total);
        leviath_core::Blueprint::new(
            "test".to_string(),
            "test agent".to_string(),
            vec![leviath_core::Stage::new(
                "main".to_string(),
                ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
            )],
            layout,
        )
    }

    fn make_engine_and_entity(
        blueprint: &leviath_core::Blueprint,
    ) -> (AgentEngine, bevy_ecs::prelude::Entity) {
        let registry = ProviderRegistry::new();
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(blueprint.clone());
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();
        (engine, entity)
    }

    #[test]
    fn initialize_context_window_creates_regions_from_blueprint() {
        let bp = make_blueprint_with_regions(vec![
            RegionDefinition::new("system".to_string(), RegionKind::Pinned, 2000),
            RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::SlidingWindow {
                    max_items: 10,
                    eviction_strategy: EvictionStrategy::PerItem,
                },
                10000,
            ),
        ]);
        let (mut engine, entity) = make_engine_and_entity(&bp);

        initialize_context_window(&mut engine, entity, &bp, "my test task");

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        // Should have the 2 defined regions + tool_results (auto-added)
        assert!(window.get_region("system").is_some());
        assert!(window.get_region("conversation").is_some());
        assert!(window.get_region("tool_results").is_some());

        // Task should be injected into the pinned region
        let sys = window.get_region("system").unwrap();
        assert_eq!(sys.content.len(), 1);
        assert!(sys.content[0].content.contains("my test task"));
    }

    #[test]
    fn initialize_context_window_adds_default_regions_if_missing() {
        // Blueprint with only a custom region (no conversation, no tool_results)
        let bp = make_blueprint_with_regions(vec![RegionDefinition::new(
            "custom".to_string(),
            RegionKind::Temporary,
            5000,
        )]);
        let (mut engine, entity) = make_engine_and_entity(&bp);

        initialize_context_window(&mut engine, entity, &bp, "task");

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        // Should auto-add conversation and tool_results
        assert!(window.get_region("tool_results").is_some());
        assert!(window.get_region("conversation").is_some());
    }

    #[test]
    fn initialize_context_window_skips_default_regions_already_present() {
        // Blueprint that already defines both auto-added regions explicitly,
        // so `initialize_context_window` must not add duplicates.
        let bp = make_blueprint_with_regions(vec![
            RegionDefinition::new("tool_results".to_string(), RegionKind::Temporary, 1234),
            RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::SlidingWindow {
                    max_items: 5,
                    eviction_strategy: EvictionStrategy::PerItem,
                },
                4321,
            ),
        ]);
        let (mut engine, entity) = make_engine_and_entity(&bp);

        initialize_context_window(&mut engine, entity, &bp, "task");

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        let tool_results: Vec<_> = window
            .regions
            .iter()
            .filter(|r| r.name == "tool_results")
            .collect();
        let conversation: Vec<_> = window
            .regions
            .iter()
            .filter(|r| r.name == "conversation")
            .collect();
        // Must not add a duplicate region.
        assert_eq!(tool_results.len(), 1);
        assert_eq!(conversation.len(), 1);
    }

    #[test]
    fn swap_context_layout_preserves_existing_content() {
        let bp = make_blueprint_with_regions(vec![
            RegionDefinition::new("system".to_string(), RegionKind::Pinned, 2000),
            RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::SlidingWindow {
                    max_items: 10,
                    eviction_strategy: EvictionStrategy::PerItem,
                },
                10000,
            ),
        ]);
        let (mut engine, entity) = make_engine_and_entity(&bp);
        initialize_context_window(&mut engine, entity, &bp, "task");

        // Add some content
        let mut window = engine.world_mut().get_mut::<ContextWindow>(entity).unwrap();
        let _ = window.add_to_region("conversation", "existing content".to_string(), 5);

        // Swap to a new layout that keeps conversation but adds scratch
        let new_layout = ContextLayout::new(
            vec![
                RegionDefinition::new("system".to_string(), RegionKind::Pinned, 3000),
                RegionDefinition::new(
                    "conversation".to_string(),
                    RegionKind::SlidingWindow {
                        max_items: 20,
                        eviction_strategy: EvictionStrategy::PerItem,
                    },
                    15000,
                ),
                RegionDefinition::new("scratch".to_string(), RegionKind::Clearable, 5000),
            ],
            23000,
        );

        swap_context_layout(&mut engine, entity, &new_layout);

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        // conversation content should be preserved
        let conv = window.get_region("conversation").unwrap();
        assert!(conv.content.iter().any(|e| e.content == "existing content"));

        // new region should exist
        assert!(window.get_region("scratch").is_some());

        // old tool_results should be gone (not in new layout)
        assert!(window.get_region("tool_results").is_none());
    }

    #[test]
    fn default_title_model_returns_correct_models() {
        assert_eq!(
            default_title_model("anthropic"),
            "claude-haiku-4-5-20251001"
        );
        assert_eq!(
            default_title_model("claude-code"),
            "claude-haiku-4-5-20251001"
        );
        assert_eq!(default_title_model("openai"), "gpt-5.4-mini");
        assert_eq!(default_title_model("google"), "gemini-3.5-flash");
        assert_eq!(
            default_title_model("openrouter"),
            "anthropic/claude-haiku-4-5"
        );
        assert_eq!(default_title_model("ollama"), "");
        assert_eq!(default_title_model("unknown"), "");
    }

    #[test]
    fn build_context_snapshot_captures_state() {
        let bp = make_blueprint_with_regions(vec![
            RegionDefinition::new("system".to_string(), RegionKind::Pinned, 2000),
            RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::SlidingWindow {
                    max_items: 10,
                    eviction_strategy: EvictionStrategy::PerItem,
                },
                10000,
            ),
        ]);
        let (mut engine, entity) = make_engine_and_entity(&bp);
        initialize_context_window(&mut engine, entity, &bp, "task");

        let mut window = engine.world_mut().get_mut::<ContextWindow>(entity).unwrap();
        let _ = window.add_to_region("conversation", "hello".to_string(), 2);

        let snap = build_context_snapshot(&engine, entity, "main").unwrap();
        assert_eq!(snap.stage_name, "main");
        assert!(snap.regions.len() >= 2);
        // Find conversation region in snapshot
        let conv_snap = snap
            .regions
            .iter()
            .find(|r| r.name == "conversation")
            .unwrap();
        assert_eq!(conv_snap.kind, "sliding");
        assert!(conv_snap.entries.iter().any(|e| e.content == "hello"));
    }

    #[test]
    fn write_context_snapshot_if_bg_is_noop_for_foreground() {
        let bp = make_blueprint_with_regions(vec![RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            2000,
        )]);
        let (mut engine, entity) = make_engine_and_entity(&bp);
        initialize_context_window(&mut engine, entity, &bp, "task");

        // Should not panic or error with None run_id
        write_context_snapshot_if_bg(&engine, entity, "main", &None);
    }

    // ─── swap_context_layout: new layout drops old regions ──────────────

    #[test]
    fn swap_context_layout_drops_unlisted_regions() {
        let bp = make_blueprint_with_regions(vec![
            RegionDefinition::new("system".to_string(), RegionKind::Pinned, 2000),
            RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::SlidingWindow {
                    max_items: 10,
                    eviction_strategy: EvictionStrategy::PerItem,
                },
                10000,
            ),
        ]);
        let (mut engine, entity) = make_engine_and_entity(&bp);
        initialize_context_window(&mut engine, entity, &bp, "task");

        // New layout only has system — conversation and tool_results should be dropped
        let new_layout = ContextLayout::new(
            vec![RegionDefinition::new(
                "system".to_string(),
                RegionKind::Pinned,
                3000,
            )],
            3000,
        );

        swap_context_layout(&mut engine, entity, &new_layout);

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        assert!(window.get_region("system").is_some());
        assert!(window.get_region("conversation").is_none());
        assert!(window.get_region("tool_results").is_none());
    }

    // ─── swap_context_layout: adds new regions ──────────────────────────

    #[test]
    fn swap_context_layout_adds_new_regions() {
        let bp = make_blueprint_with_regions(vec![RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            2000,
        )]);
        let (mut engine, entity) = make_engine_and_entity(&bp);
        initialize_context_window(&mut engine, entity, &bp, "task");

        let new_layout = ContextLayout::new(
            vec![
                RegionDefinition::new("system".to_string(), RegionKind::Pinned, 2000),
                RegionDefinition::new("scratch".to_string(), RegionKind::Clearable, 5000),
                RegionDefinition::new("notes".to_string(), RegionKind::Temporary, 3000),
            ],
            10000,
        );

        swap_context_layout(&mut engine, entity, &new_layout);

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        assert!(window.get_region("scratch").is_some());
        assert!(window.get_region("notes").is_some());
    }

    #[test]
    fn swap_context_layout_missing_component_is_noop() {
        let new_layout = ContextLayout::new(
            vec![RegionDefinition::new(
                "system".to_string(),
                RegionKind::Pinned,
                2000,
            )],
            2000,
        );
        let registry = ProviderRegistry::new();
        let mut engine = AgentEngine::with_providers(registry);
        // Bare entity, no `ContextWindow` component, to exercise the `if let
        // Some(..) = get_mut(..)` `None` branch — should not panic.
        let entity = engine.world_mut().spawn(()).id();

        swap_context_layout(&mut engine, entity, &new_layout);

        assert!(engine.world().get::<ContextWindow>(entity).is_none());
    }

    // ─── initialize_context_window: no pinned region ────────────────────

    #[test]
    fn initialize_context_window_no_pinned_region() {
        let bp = make_blueprint_with_regions(vec![RegionDefinition::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 10,
                eviction_strategy: EvictionStrategy::PerItem,
            },
            10000,
        )]);
        let (mut engine, entity) = make_engine_and_entity(&bp);

        // No pinned region means task won't be injected into system, but should not panic
        initialize_context_window(&mut engine, entity, &bp, "task");

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        assert!(window.get_region("conversation").is_some());
    }

    #[test]
    fn initialize_context_window_missing_component_is_noop() {
        let bp = make_blueprint_with_regions(vec![RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            2000,
        )]);
        let registry = ProviderRegistry::new();
        let mut engine = AgentEngine::with_providers(registry);
        // Bare entity, no `ContextWindow` component, to exercise the `if let
        // Some(..) = get_mut(..)` `None` branch — should not panic.
        let entity = engine.world_mut().spawn(()).id();

        initialize_context_window(&mut engine, entity, &bp, "task");

        assert!(engine.world().get::<ContextWindow>(entity).is_none());
    }

    // ─── build_context_snapshot: region kind strings ────────────────────

    #[test]
    fn build_context_snapshot_region_kinds() {
        let bp = make_blueprint_with_regions(vec![
            RegionDefinition::new("system".to_string(), RegionKind::Pinned, 2000),
            RegionDefinition::new("temp".to_string(), RegionKind::Temporary, 1000),
            RegionDefinition::new("clear".to_string(), RegionKind::Clearable, 1000),
            RegionDefinition::new(
                "compact".to_string(),
                RegionKind::Compacting {
                    threshold_tokens: 500,
                },
                1000,
            ),
            RegionDefinition::new(
                "history".to_string(),
                RegionKind::CompactHistory {
                    source_region: "compact".to_string(),
                },
                1000,
            ),
        ]);
        let (mut engine, entity) = make_engine_and_entity(&bp);
        initialize_context_window(&mut engine, entity, &bp, "task");

        let snap = build_context_snapshot(&engine, entity, "test").unwrap();
        let kinds: Vec<&str> = snap.regions.iter().map(|r| r.kind.as_str()).collect();
        assert!(kinds.contains(&"pinned"));
        assert!(kinds.contains(&"temporary"));
        assert!(kinds.contains(&"clearable"));
        assert!(kinds.contains(&"compacting"));
        assert!(kinds.contains(&"history"));
    }

    // ─── build_context_snapshot: returns None for invalid entity ────────

    #[test]
    fn build_context_snapshot_invalid_entity() {
        let bp = make_blueprint_with_regions(vec![RegionDefinition::new(
            "system".to_string(),
            RegionKind::Pinned,
            2000,
        )]);
        let (engine, _entity) = make_engine_and_entity(&bp);

        // Create a fake entity that doesn't have a ContextWindow
        let fake_entity = bevy_ecs::prelude::Entity::from_raw(9999);
        let snap = build_context_snapshot(&engine, fake_entity, "test");
        assert!(snap.is_none());
    }

    // ─── default_title_model: all known providers ───────────────────────

    #[test]
    fn default_title_model_empty_for_unknown() {
        assert_eq!(default_title_model("custom-provider"), "");
        assert_eq!(default_title_model(""), "");
    }

    // ─── initialize_context_window: empty task ──────────────────────────

    #[test]
    fn initialize_context_window_empty_task() {
        let bp = make_blueprint_with_regions(vec![
            RegionDefinition::new("system".to_string(), RegionKind::Pinned, 2000),
            RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::SlidingWindow {
                    max_items: 10,
                    eviction_strategy: EvictionStrategy::PerItem,
                },
                10000,
            ),
        ]);
        let (mut engine, entity) = make_engine_and_entity(&bp);
        initialize_context_window(&mut engine, entity, &bp, "");

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        let sys = window.get_region("system").unwrap();
        // Empty task still gets added
        assert_eq!(sys.content.len(), 1);
    }

    // ─── generate_title: fenced-code-block regression ───────────────────
    //
    // When the model wraps its answer in a markdown fence, taking only the
    // first line of the response used to grab the fence/language-tag line
    // itself (e.g. "```python") instead of the actual title on the next
    // line — confirmed via real run-state data showing title: "python" for
    // a task about downloading webpages, instead of a real title.

    #[tokio::test]
    async fn generate_title_unwraps_fenced_code_block_with_language_tag() {
        let registry = make_title_registry("```python\nWeb Page Downloader\n```");
        let config = make_title_config();
        let title = generate_title("download a webpage", &config, &registry, None).await;
        assert_eq!(title, Some("Web Page Downloader".to_string()));
    }

    #[tokio::test]
    async fn generate_title_unwraps_fenced_code_block_without_language_tag() {
        let registry = make_title_registry("```\nDownload Webpage Tool\n```");
        let config = make_title_config();
        let title = generate_title("download a webpage", &config, &registry, None).await;
        assert_eq!(title, Some("Download Webpage Tool".to_string()));
    }

    #[tokio::test]
    async fn generate_title_fenced_block_with_no_real_content_returns_none() {
        // Just a fence + language tag, no actual title text on a second line.
        let registry = make_title_registry("```python");
        let config = make_title_config();
        let title = generate_title("download a webpage", &config, &registry, None).await;
        assert_eq!(title, None);
    }

    #[tokio::test]
    async fn generate_title_plain_response_unaffected() {
        let registry = make_title_registry("Web Page Downloader Script");
        let config = make_title_config();
        let title = generate_title("download a webpage", &config, &registry, None).await;
        assert_eq!(title, Some("Web Page Downloader Script".to_string()));
    }

    #[tokio::test]
    async fn generate_title_strips_markdown_heading_and_quotes() {
        let registry = make_title_registry("# \"Web Page Downloader\"");
        let config = make_title_config();
        let title = generate_title("download a webpage", &config, &registry, None).await;
        assert_eq!(title, Some("Web Page Downloader".to_string()));
    }

    #[tokio::test]
    async fn generate_title_empty_response_returns_none() {
        let registry = make_title_registry("");
        let config = make_title_config();
        let title = generate_title("download a webpage", &config, &registry, None).await;
        assert_eq!(title, None);
    }

    #[tokio::test]
    async fn generate_title_missing_provider_returns_none() {
        let registry = ProviderRegistry::new(); // "mock" not registered
        let config = make_title_config();
        let title = generate_title("download a webpage", &config, &registry, None).await;
        assert_eq!(title, None);
    }

    // ─── generate_title: provider/model resolution fallback branches ────

    #[tokio::test]
    async fn generate_title_falls_back_to_default_provider_when_title_provider_unset() {
        // config.title.provider = None -> falls back to config.default_provider
        let registry = make_title_registry("Web Page Downloader");
        let config = Config {
            default_provider: "mock".to_string(),
            title: crate::config::TitleConfig {
                enabled: true,
                provider: None,
                model: Some("mock-model".to_string()),
            },
            ..Config::default()
        };
        let title = generate_title("download a webpage", &config, &registry, None).await;
        assert_eq!(title, Some("Web Page Downloader".to_string()));
    }

    #[tokio::test]
    async fn generate_title_uses_default_title_model_when_title_model_unset_for_known_provider() {
        // config.title.model = None, provider is "anthropic" (known to
        // default_title_model) -> resolves via default_title_model, not
        // fallback_model. Registered under "anthropic" so the real provider
        // name (not "mock") is what's looked up.
        let mut registry = ProviderRegistry::new();
        registry.register(
            "anthropic".to_string(),
            std::sync::Arc::new(CannedTitleProvider {
                content: "Web Page Downloader".to_string(),
            }),
        );
        let config = Config {
            default_provider: "anthropic".to_string(),
            title: crate::config::TitleConfig {
                enabled: true,
                provider: None,
                model: None,
            },
            ..Config::default()
        };
        let title = generate_title("download a webpage", &config, &registry, None).await;
        assert_eq!(title, Some("Web Page Downloader".to_string()));
    }

    #[tokio::test]
    async fn generate_title_uses_fallback_model_for_unknown_provider_when_title_model_unset() {
        // config.title.model = None, provider unknown to default_title_model
        // (returns "") -> falls through to fallback_model.
        let registry = make_title_registry("Web Page Downloader");
        let config = Config {
            default_provider: "mock".to_string(),
            title: crate::config::TitleConfig {
                enabled: true,
                provider: None,
                model: None,
            },
            ..Config::default()
        };
        let title = generate_title(
            "download a webpage",
            &config,
            &registry,
            Some("fallback-model"),
        )
        .await;
        assert_eq!(title, Some("Web Page Downloader".to_string()));
    }

    #[tokio::test]
    async fn generate_title_returns_none_when_no_model_resolves_at_all() {
        // title.model unset, provider unknown to default_title_model, and no
        // fallback_model given -> the `?` on model resolution short-circuits.
        let registry = make_title_registry("Web Page Downloader");
        let config = Config {
            default_provider: "mock".to_string(),
            title: crate::config::TitleConfig {
                enabled: true,
                provider: None,
                model: None,
            },
            ..Config::default()
        };
        let title = generate_title("download a webpage", &config, &registry, None).await;
        assert_eq!(title, None);
    }

    // ─── generate_title: inference error ─────────────────────────────────

    struct FailingTitleProvider;

    #[async_trait]
    impl Provider for FailingTitleProvider {
        async fn infer(
            &self,
            _request: InferenceRequest,
        ) -> Result<InferenceResponse, ProviderError> {
            Err(ProviderError::Other("simulated failure".to_string()))
        }

        fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len() / 4
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            100_000
        }

        fn name(&self) -> &str {
            "failing-mock"
        }

        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }

    #[tokio::test]
    async fn generate_title_infer_error_returns_none() {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "mock".to_string(),
            std::sync::Arc::new(FailingTitleProvider),
        );
        let config = make_title_config();
        let title = generate_title("download a webpage", &config, &registry, None).await;
        assert_eq!(title, None);
    }

    #[test]
    fn failing_title_provider_trivial_trait_methods() {
        let provider = FailingTitleProvider;
        assert_eq!(provider.count_tokens("abcd", "mock-model"), 1);
        assert_eq!(provider.max_context_tokens("mock-model"), 100_000);
        assert_eq!(provider.name(), "failing-mock");
        let _ = provider.capabilities("mock-model");
    }

    // ─── generate_title: title.is_empty() after stripping punctuation ────

    #[tokio::test]
    async fn generate_title_returns_none_when_only_punctuation_remains_after_stripping() {
        // Raw response is nothing but quote marks -- after all trim_start/
        // trim_end passes, `title` is empty via the explicit
        // `if title.is_empty()` branch, distinct from the `?`-short-circuit
        // path already covered by the "no real content" fenced-block test.
        let registry = make_title_registry("\"\"");
        let config = make_title_config();
        let title = generate_title("download a webpage", &config, &registry, None).await;
        assert_eq!(title, None);
    }

    // ─── CannedTitleProvider: otherwise-dead trivial trait methods ───────

    #[test]
    fn canned_title_provider_trivial_trait_methods() {
        let provider = CannedTitleProvider {
            content: "x".to_string(),
        };
        assert_eq!(provider.count_tokens("abcd", "mock-model"), 1);
        assert_eq!(provider.max_context_tokens("mock-model"), 100_000);
        assert_eq!(provider.name(), "mock");
        let _ = provider.capabilities("mock-model");
    }

    // ─── record_stage_output / record_stage_log ──────────────────────────

    #[test]
    fn record_stage_output_and_log_write_through_to_runstate() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "record_stage_output_and_log_write_through_to_runstate",
        );
        let run_id = "test-helpers-record-stage";
        let dir = runstate::run_dir(run_id);
        let _ = std::fs::create_dir_all(&dir);

        record_stage_output(run_id, 0, "some output line");
        record_stage_log(run_id, 0, "some log line");

        let output = runstate::tail_stage_output(run_id, 0, 65536);
        let log = runstate::tail_stage_log(run_id, 0, 65536);
        assert!(output.contains("some output line"));
        assert!(log.contains("some log line"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
