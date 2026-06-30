//! Graph traversal: transition resolution, edge transforms, compaction.

use leviath_core::blueprint::{EdgeTransform, StageResult, TransitionCondition, TransitionEdge};
use leviath_core::lifecycle::CompactionConfig;
use leviath_core::{Blueprint, RegionKind, Stage};
use leviath_runtime::{AgentEngine, ContextWindow};
use std::collections::HashMap;

/// Determine whether a blueprint uses graph mode (any stage has transitions set).
pub fn is_graph_mode(blueprint: &Blueprint) -> bool {
    blueprint.stages.iter().any(|s| s.transitions.is_some())
}

/// Resolve the next transition from a stage, considering conditions, visit counts,
/// and LLM routing when multiple edges are available.
///
/// Returns `None` when the stage is terminal (no valid outgoing transitions).
#[allow(clippy::too_many_arguments)]
pub async fn resolve_transition(
    stage: &Stage,
    stage_idx: usize,
    blueprint: &Blueprint,
    visit_counts: &HashMap<String, usize>,
    stage_result: &StageResult,
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
) -> Option<(TransitionEdge, usize)> {
    match &stage.transitions {
        None => {
            // Linear mode: advance to next stage by index
            if stage_idx + 1 < blueprint.stages.len() {
                let next = &blueprint.stages[stage_idx + 1];
                let next_idx = stage_idx + 1;
                Some((
                    TransitionEdge {
                        target: next.name.clone(),
                        condition: TransitionCondition::Always,
                        hint: None,
                        transform: EdgeTransform::Direct,
                    },
                    next_idx,
                ))
            } else {
                None // terminal
            }
        }
        Some(transitions) => {
            if transitions.is_empty() {
                return None; // terminal stage
            }

            // Filter edges by visit count limits
            let available: Vec<(&String, &TransitionEdge)> = transitions
                .iter()
                .filter(|(target_name, _edge)| {
                    let target_stage = blueprint.find_stage(target_name);
                    if let Some(ts) = target_stage {
                        if let Some(max_rev) = ts.max_revisits {
                            let visits =
                                visit_counts.get(target_name.as_str()).copied().unwrap_or(0);
                            visits <= max_rev // allow first visit + max_revisits
                        } else {
                            true
                        }
                    } else {
                        false // unknown target, skip
                    }
                })
                .collect();

            if available.is_empty() {
                return None; // all targets exhausted
            }

            // Step 1: Error condition — auto-transition if error occurred
            if *stage_result == StageResult::Error {
                if let Some((_name, edge)) = available
                    .iter()
                    .find(|(_, e)| e.condition == TransitionCondition::Error)
                {
                    let target_idx = blueprint
                        .stages
                        .iter()
                        .position(|s| s.name == edge.target)
                        .unwrap_or(0);
                    return Some(((*edge).clone(), target_idx));
                }
            }

            // Step 2: MaxIterations condition — auto-transition
            if *stage_result == StageResult::MaxIterations {
                if let Some((_name, edge)) = available
                    .iter()
                    .find(|(_, e)| e.condition == TransitionCondition::MaxIterations)
                {
                    let target_idx = blueprint
                        .stages
                        .iter()
                        .position(|s| s.name == edge.target)
                        .unwrap_or(0);
                    return Some(((*edge).clone(), target_idx));
                }
            }

            // Step 3: Filter to only always/llm_choice edges for LLM prompt
            let choosable: Vec<(&String, &TransitionEdge)> = available
                .into_iter()
                .filter(|(_, e)| {
                    matches!(
                        e.condition,
                        TransitionCondition::Always | TransitionCondition::LlmChoice
                    )
                })
                .collect();

            match choosable.len() {
                0 => None, // terminal
                1 => {
                    let (_, edge) = choosable[0];
                    let target_idx = blueprint
                        .stages
                        .iter()
                        .position(|s| s.name == edge.target)
                        .unwrap_or(0);
                    Some((edge.clone(), target_idx))
                }
                _ => {
                    // Multiple edges: prompt LLM to choose
                    let chosen = prompt_llm_transition(
                        stage,
                        &choosable,
                        engine,
                        entity,
                        provider_name,
                        model_name,
                    )
                    .await;
                    match chosen {
                        Some(edge) => {
                            let target_idx = blueprint
                                .stages
                                .iter()
                                .position(|s| s.name == edge.target)
                                .unwrap_or(0);
                            Some((edge, target_idx))
                        }
                        None => {
                            // LLM didn't pick a valid target — treat as terminal
                            None
                        }
                    }
                }
            }
        }
    }
}

/// Prompt the LLM to choose between multiple transition edges.
pub async fn prompt_llm_transition(
    stage: &Stage,
    edges: &[(&String, &TransitionEdge)],
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
) -> Option<TransitionEdge> {
    // Build the transition prompt
    let prompt = if let Some(ref custom_prompt) = stage.transition_prompt {
        let mut p = custom_prompt.clone();
        p.push_str("\n\nAvailable transitions:\n");
        for (name, edge) in edges {
            p.push_str(&format!("- {}", name));
            if let Some(ref hint) = edge.hint {
                p.push_str(&format!(": {}", hint));
            }
            p.push('\n');
        }
        p.push_str("\nRespond with ONLY the stage name you want to transition to, nothing else.");
        p
    } else {
        let mut p = format!(
            "Stage '{}' is complete. Available next stages:\n",
            stage.name
        );
        for (name, edge) in edges {
            p.push_str(&format!("- {}", name));
            if let Some(ref hint) = edge.hint {
                p.push_str(&format!(": {}", hint));
            }
            p.push('\n');
        }
        p.push_str("\nWhich stage should run next? Respond with ONLY the stage name.");
        p
    };

    // Inject the transition prompt into the context window
    if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
        let tokens = prompt.len() / 4 + 1;
        let _ = window.add_to_region("conversation", format!("User: {}", prompt), tokens);
    }

    // Run a single inference call to get the LLM's choice
    let provider = engine.get_provider(provider_name)?;
    let (messages, max_tokens) = {
        let window = engine.world().get::<ContextWindow>(entity)?;
        let messages = window.assemble_messages();
        let remaining = window.max_tokens.saturating_sub(window.current_tokens);
        let max_tokens = remaining.min(256); // short response expected
        (messages, max_tokens)
    };

    let temperature = 0.0; // deterministic for routing

    let request = leviath_providers::InferenceRequest {
        messages,
        model: model_name.to_string(),
        max_tokens,
        temperature,
        tools: Vec::new(),
        extra: serde_json::Value::Null,
    };

    let response = provider.infer(request).await.ok()?;
    let choice = response.content.trim().to_string();

    // Add the LLM's response to context
    if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
        let tokens = choice.len() / 4 + 1;
        let _ = window.add_to_region(
            "conversation",
            format!("Assistant: Transitioning to: {}", choice),
            tokens,
        );
    }

    // Match the response to an available edge
    for (name, edge) in edges {
        if choice.eq_ignore_ascii_case(name) || choice.contains(name.as_str()) {
            return Some((*edge).clone());
        }
    }

    // Fuzzy fallback: check if any edge target is contained in the response
    for (name, edge) in edges {
        if choice.to_lowercase().contains(&name.to_lowercase()) {
            return Some((*edge).clone());
        }
    }

    // If nothing matched, pick the first edge as fallback
    tracing::warn!(
        stage = %stage.name,
        llm_response = %choice,
        "LLM transition response didn't match any edge — using first available"
    );
    Some(edges.first()?.1.clone())
}

/// Apply an edge transform to the context window before entering the next stage.
pub async fn apply_edge_transform(
    edge: &TransitionEdge,
    visit_counts: &HashMap<String, usize>,
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    compaction_config: Option<&CompactionConfig>,
) {
    let visits = visit_counts.get(&edge.target).copied().unwrap_or(0);

    // Default: Direct for first visit, Compact for revisits (when no explicit transform)
    let effective_transform = match &edge.transform {
        EdgeTransform::Direct if visits > 0 => {
            // Revisit with no explicit transform: use compact
            &EdgeTransform::Compact { prompt: None }
        }
        other => other,
    };

    match effective_transform {
        EdgeTransform::Direct => {
            // No-op: context carries forward as-is
        }
        EdgeTransform::Clear => {
            // Clear all non-pinned regions
            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                for region in &mut window.regions {
                    if !matches!(
                        region.kind,
                        RegionKind::Pinned | RegionKind::CompactHistory { .. }
                    ) {
                        region.clear();
                    }
                }
                window.current_tokens = window.calculate_tokens();
            }
        }
        EdgeTransform::Compact { prompt } => {
            // LLM-summarize conversation/scratch regions
            let compact_prompt = prompt.clone().unwrap_or_else(|| {
                format!(
                    "Summarize the conversation so far as context for the next stage '{}'. \
                     Keep key decisions, findings, and action items. Be concise.",
                    edge.target
                )
            });
            apply_compact_transform(
                engine,
                entity,
                provider_name,
                model_name,
                &compact_prompt,
                compaction_config,
            )
            .await;
        }
        EdgeTransform::Custom {
            carry: _,
            compact,
            clear,
            compact_prompt,
        } => {
            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                // Clear specified regions
                for region in &mut window.regions {
                    if clear.contains(&region.name) {
                        region.clear();
                    }
                }
                window.current_tokens = window.calculate_tokens();
            }

            // Compact specified regions
            if !compact.is_empty() {
                let prompt = compact_prompt.clone().unwrap_or_else(|| {
                    format!(
                        "Summarize the content from regions [{}] as context for stage '{}'.",
                        compact.join(", "),
                        edge.target
                    )
                });
                apply_compact_transform(
                    engine,
                    entity,
                    provider_name,
                    model_name,
                    &prompt,
                    compaction_config,
                )
                .await;
            }

            // carry regions are left untouched (they carry forward as-is)
        }
    }
}

/// Run LLM compaction on the conversation/compacting regions.
pub async fn apply_compact_transform(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    prompt: &str,
    compaction_config: Option<&CompactionConfig>,
) {
    // Use the compaction provider/model if configured, otherwise fall back to the stage's
    let (compact_provider, compact_model) = if let Some(cc) = compaction_config {
        (cc.provider.as_str(), cc.model.as_str())
    } else {
        (provider_name, model_name)
    };

    let provider = match engine.get_provider(compact_provider) {
        Some(p) => p,
        None => return,
    };

    // Gather content from compactable regions
    let content_to_compact = {
        let window = match engine.world().get::<ContextWindow>(entity) {
            Some(w) => w,
            None => return,
        };
        let mut parts = Vec::new();
        for region in &window.regions {
            if matches!(
                region.kind,
                RegionKind::SlidingWindow { .. }
                    | RegionKind::Compacting { .. }
                    | RegionKind::Temporary
                    | RegionKind::Clearable
            ) && !region.content.is_empty()
            {
                let region_content: String = region
                    .content
                    .iter()
                    .map(|e| e.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                parts.push(format!("[{}]:\n{}", region.name, region_content));
            }
        }
        parts.join("\n\n")
    };

    if content_to_compact.is_empty() {
        return;
    }

    let messages = vec![
        leviath_providers::Message {
            role: "system".to_string(),
            content: prompt.to_string(),
            cache_breakpoint: false,
        },
        leviath_providers::Message {
            role: "user".to_string(),
            content: content_to_compact,
            cache_breakpoint: false,
        },
    ];

    let max_summary_tokens = compaction_config
        .map(|cc| cc.max_summary_tokens)
        .unwrap_or(2000);

    let request = leviath_providers::InferenceRequest {
        messages,
        model: compact_model.to_string(),
        max_tokens: max_summary_tokens,
        temperature: compaction_config.map(|cc| cc.temperature).unwrap_or(0.3),
        tools: Vec::new(),
        extra: serde_json::Value::Null,
    };

    match provider.infer(request).await {
        Ok(response) => {
            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                // Clear compactable regions
                for region in &mut window.regions {
                    if matches!(
                        region.kind,
                        RegionKind::SlidingWindow { .. }
                            | RegionKind::Compacting { .. }
                            | RegionKind::Temporary
                            | RegionKind::Clearable
                    ) {
                        region.clear();
                    }
                }
                // Add the summary to conversation
                let tokens = response.content.len() / 4 + 1;
                let _ = window.add_to_region(
                    "conversation",
                    format!(
                        "[Context summary from previous stage]: {}",
                        response.content
                    ),
                    tokens,
                );
                window.current_tokens = window.calculate_tokens();
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to compact context during edge transform");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::blueprint::ModelConfig;
    use leviath_core::layout::RegionDefinition;
    use leviath_core::ContextLayout;
    use leviath_runtime::{AgentPool, ProviderRegistry};

    /// Helper to create a minimal blueprint for testing.
    fn make_blueprint(stages: Vec<Stage>) -> Blueprint {
        let layout = ContextLayout::new(
            vec![
                RegionDefinition::new("system".to_string(), RegionKind::Pinned, 2000),
                RegionDefinition::new(
                    "conversation".to_string(),
                    RegionKind::SlidingWindow { max_items: 10 },
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
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
        )
    }

    fn make_engine_and_entity(blueprint: &Blueprint) -> (AgentEngine, bevy_ecs::prelude::Entity) {
        let registry = ProviderRegistry::new();
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(blueprint.clone());
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();

        // Initialize context window
        crate::commands::run::helpers::initialize_context_window(
            &mut engine,
            entity,
            blueprint,
            "test task",
        );

        (engine, entity)
    }

    // ─── is_graph_mode ───────────────────────────────────────────────────────

    #[test]
    fn is_graph_mode_with_transitions_returns_true() {
        let mut stage = make_stage("main");
        stage.transitions = Some(HashMap::new());
        let bp = make_blueprint(vec![stage]);
        assert!(is_graph_mode(&bp));
    }

    #[test]
    fn is_graph_mode_without_transitions_returns_false() {
        let bp = make_blueprint(vec![make_stage("main")]);
        assert!(!is_graph_mode(&bp));
    }

    #[test]
    fn is_graph_mode_mixed_stages() {
        let mut stage1 = make_stage("a");
        stage1.transitions = Some(HashMap::new());
        let stage2 = make_stage("b");
        let bp = make_blueprint(vec![stage1, stage2]);
        assert!(is_graph_mode(&bp));
    }

    // ─── resolve_transition ──────────────────────────────────────────────────

    #[tokio::test]
    async fn resolve_transition_linear_mode_advances_to_next() {
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b"), make_stage("c")]);
        let (mut engine, entity) = make_engine_and_entity(&bp);
        let visit_counts = HashMap::new();

        let result = resolve_transition(
            &bp.stages[0],
            0,
            &bp,
            &visit_counts,
            &StageResult::Success,
            &mut engine,
            entity,
            "anthropic",
            "claude-sonnet-4-6",
        )
        .await;

        let (edge, idx) = result.unwrap();
        assert_eq!(edge.target, "b");
        assert_eq!(idx, 1);
    }

    #[tokio::test]
    async fn resolve_transition_linear_mode_terminal() {
        let bp = make_blueprint(vec![make_stage("a")]);
        let (mut engine, entity) = make_engine_and_entity(&bp);
        let visit_counts = HashMap::new();

        let result = resolve_transition(
            &bp.stages[0],
            0,
            &bp,
            &visit_counts,
            &StageResult::Success,
            &mut engine,
            entity,
            "anthropic",
            "claude-sonnet-4-6",
        )
        .await;

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn resolve_transition_single_always_edge() {
        let mut stage_a = make_stage("a");
        let stage_b = make_stage("b");
        let mut transitions = HashMap::new();
        transitions.insert(
            "b".to_string(),
            TransitionEdge {
                target: "b".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
            },
        );
        stage_a.transitions = Some(transitions);

        let bp = make_blueprint(vec![stage_a, stage_b]);
        let (mut engine, entity) = make_engine_and_entity(&bp);
        let visit_counts = HashMap::new();

        let result = resolve_transition(
            &bp.stages[0],
            0,
            &bp,
            &visit_counts,
            &StageResult::Success,
            &mut engine,
            entity,
            "anthropic",
            "claude-sonnet-4-6",
        )
        .await;

        let (edge, idx) = result.unwrap();
        assert_eq!(edge.target, "b");
        assert_eq!(idx, 1);
    }

    #[tokio::test]
    async fn resolve_transition_error_condition_routes_to_error_handler() {
        let mut stage_a = make_stage("a");
        let stage_b = make_stage("b");
        let stage_err = make_stage("error_handler");
        let mut transitions = HashMap::new();
        transitions.insert(
            "b".to_string(),
            TransitionEdge {
                target: "b".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
            },
        );
        transitions.insert(
            "error_handler".to_string(),
            TransitionEdge {
                target: "error_handler".to_string(),
                condition: TransitionCondition::Error,
                hint: None,
                transform: EdgeTransform::Direct,
            },
        );
        stage_a.transitions = Some(transitions);

        let bp = make_blueprint(vec![stage_a, stage_b, stage_err]);
        let (mut engine, entity) = make_engine_and_entity(&bp);
        let visit_counts = HashMap::new();

        let result = resolve_transition(
            &bp.stages[0],
            0,
            &bp,
            &visit_counts,
            &StageResult::Error,
            &mut engine,
            entity,
            "anthropic",
            "claude-sonnet-4-6",
        )
        .await;

        let (edge, _idx) = result.unwrap();
        assert_eq!(edge.target, "error_handler");
    }

    #[tokio::test]
    async fn resolve_transition_max_iterations_condition() {
        let mut stage_a = make_stage("a");
        let stage_b = make_stage("b");
        let stage_timeout = make_stage("timeout");
        let mut transitions = HashMap::new();
        transitions.insert(
            "b".to_string(),
            TransitionEdge {
                target: "b".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
            },
        );
        transitions.insert(
            "timeout".to_string(),
            TransitionEdge {
                target: "timeout".to_string(),
                condition: TransitionCondition::MaxIterations,
                hint: None,
                transform: EdgeTransform::Direct,
            },
        );
        stage_a.transitions = Some(transitions);

        let bp = make_blueprint(vec![stage_a, stage_b, stage_timeout]);
        let (mut engine, entity) = make_engine_and_entity(&bp);
        let visit_counts = HashMap::new();

        let result = resolve_transition(
            &bp.stages[0],
            0,
            &bp,
            &visit_counts,
            &StageResult::MaxIterations,
            &mut engine,
            entity,
            "anthropic",
            "claude-sonnet-4-6",
        )
        .await;

        let (edge, _idx) = result.unwrap();
        assert_eq!(edge.target, "timeout");
    }

    #[tokio::test]
    async fn resolve_transition_empty_transitions_is_terminal() {
        let mut stage_a = make_stage("a");
        stage_a.transitions = Some(HashMap::new());

        let bp = make_blueprint(vec![stage_a]);
        let (mut engine, entity) = make_engine_and_entity(&bp);
        let visit_counts = HashMap::new();

        let result = resolve_transition(
            &bp.stages[0],
            0,
            &bp,
            &visit_counts,
            &StageResult::Success,
            &mut engine,
            entity,
            "anthropic",
            "claude-sonnet-4-6",
        )
        .await;

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn resolve_transition_max_revisits_enforcement() {
        let mut stage_a = make_stage("a");
        let mut stage_b = make_stage("b");
        stage_b.max_revisits = Some(1); // allow first visit + 1 revisit

        let mut transitions = HashMap::new();
        transitions.insert(
            "b".to_string(),
            TransitionEdge {
                target: "b".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
            },
        );
        stage_a.transitions = Some(transitions);

        let bp = make_blueprint(vec![stage_a, stage_b]);
        let (mut engine, entity) = make_engine_and_entity(&bp);

        // First visit: ok (visits = 0 <= max_revisits = 1)
        let mut visit_counts = HashMap::new();
        let result = resolve_transition(
            &bp.stages[0],
            0,
            &bp,
            &visit_counts,
            &StageResult::Success,
            &mut engine,
            entity,
            "anthropic",
            "claude-sonnet-4-6",
        )
        .await;
        assert!(result.is_some());

        // Second visit: ok (visits = 1 <= max_revisits = 1)
        visit_counts.insert("b".to_string(), 1);
        let result = resolve_transition(
            &bp.stages[0],
            0,
            &bp,
            &visit_counts,
            &StageResult::Success,
            &mut engine,
            entity,
            "anthropic",
            "claude-sonnet-4-6",
        )
        .await;
        assert!(result.is_some());

        // Third visit: blocked (visits = 2 > max_revisits = 1)
        visit_counts.insert("b".to_string(), 2);
        let result = resolve_transition(
            &bp.stages[0],
            0,
            &bp,
            &visit_counts,
            &StageResult::Success,
            &mut engine,
            entity,
            "anthropic",
            "claude-sonnet-4-6",
        )
        .await;
        assert!(result.is_none());
    }

    // ─── apply_edge_transform ────────────────────────────────────────────────

    #[tokio::test]
    async fn apply_edge_transform_direct_is_noop() {
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, entity) = make_engine_and_entity(&bp);

        // Add some content to conversation
        if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
            let _ = window.add_to_region("conversation", "test content".to_string(), 5);
        }

        let edge = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Direct,
        };
        let visit_counts = HashMap::new();

        apply_edge_transform(
            &edge,
            &visit_counts,
            &mut engine,
            entity,
            "anthropic",
            "test",
            None,
        )
        .await;

        // Content should still be there
        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = window.get_region("conversation").unwrap();
        assert!(!conv.content.is_empty());
    }

    #[tokio::test]
    async fn apply_edge_transform_clear_clears_non_pinned() {
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, entity) = make_engine_and_entity(&bp);

        // Add content to both regions
        if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
            let _ = window.add_to_region("system", "system content".to_string(), 5);
            let _ = window.add_to_region("conversation", "conv content".to_string(), 5);
        }

        let edge = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Clear,
        };
        let visit_counts = HashMap::new();

        apply_edge_transform(
            &edge,
            &visit_counts,
            &mut engine,
            entity,
            "anthropic",
            "test",
            None,
        )
        .await;

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        // Pinned region should be preserved
        let sys = window.get_region("system").unwrap();
        assert!(!sys.content.is_empty());
        // Non-pinned should be cleared
        let conv = window.get_region("conversation").unwrap();
        assert!(conv.content.is_empty());
    }

    #[tokio::test]
    async fn apply_edge_transform_direct_becomes_compact_on_revisit() {
        // When transform is Direct but visit count > 0, it should try to compact.
        // Since we don't have a real provider, the compact will fail silently,
        // but we can verify the intent by checking the function doesn't panic.
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, entity) = make_engine_and_entity(&bp);

        let edge = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Direct,
        };
        let mut visit_counts = HashMap::new();
        visit_counts.insert("b".to_string(), 1); // revisit

        // Should not panic — compact fails silently since no provider is configured
        apply_edge_transform(
            &edge,
            &visit_counts,
            &mut engine,
            entity,
            "anthropic",
            "test",
            None,
        )
        .await;
    }
}
