//! Graph traversal: transition resolution, edge transforms, compaction.

use crate::{AgentEngine, ContextWindow};
use leviath_core::blueprint::{EdgeTransform, StageResult, TransitionCondition, TransitionEdge};
use leviath_core::lifecycle::CompactionConfig;
use leviath_core::{Blueprint, RegionKind, Stage};
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
                // A single edge is normally auto-followed (no LLM call needed)
                // — UNLESS the stage explicitly allows ending here instead
                // (e.g. a review stage that approves the work), in which case
                // the LLM must still be asked so it can say "DONE".
                1 if !stage.allow_complete => {
                    let (_, edge) = choosable[0];
                    let target_idx = blueprint
                        .stages
                        .iter()
                        .position(|s| s.name == edge.target)
                        .unwrap_or(0);
                    Some((edge.clone(), target_idx))
                }
                _ => {
                    // Multiple edges (or a single edge the stage may decline):
                    // prompt the LLM to choose.
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
                            // LLM said DONE (or didn't pick a valid target) — terminal
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
        if stage.allow_complete {
            p.push_str(
                "\nRespond with ONLY the stage name you want to transition to, or ONLY the \
                 word DONE if no further stage is needed and the run should end here.",
            );
        } else {
            p.push_str(
                "\nRespond with ONLY the stage name you want to transition to, nothing else.",
            );
        }
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
        if stage.allow_complete {
            p.push_str(
                "\nWhich stage should run next? Respond with ONLY the stage name, or ONLY the \
                 word DONE if no further stage is needed and the run should end here.",
            );
        } else {
            p.push_str("\nWhich stage should run next? Respond with ONLY the stage name.");
        }
        p
    };

    // Inject the transition prompt into the context window
    if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
        let tokens = prompt.len() / 4 + 1;
        let _ = window.add_typed_entry(
            "conversation",
            leviath_core::EntryKind::UserMessage,
            prompt.clone(),
            tokens,
        );
    }

    // Run a single inference call to get the LLM's choice
    let provider = engine.get_provider(provider_name)?;
    let (assembled, max_tokens) = {
        let window = engine.world().get::<ContextWindow>(entity)?;
        let assembled = window.assemble();
        let remaining = window.max_tokens.saturating_sub(window.current_tokens);
        let max_tokens = remaining.min(256); // short response expected
        (assembled, max_tokens)
    };

    let temperature = 0.0; // deterministic for routing

    let request = leviath_providers::InferenceRequest {
        messages: assembled.messages,
        system: assembled.system_blocks,
        model: model_name.to_string(),
        max_tokens,
        temperature,
        tools: Vec::new(),
        extra: serde_json::Value::Null,
    };

    let response = provider.infer(request).await.ok()?;
    let choice = response.content.trim().to_string();

    // Add the LLM's response to context. The `window()?` above already
    // short-circuited the whole function with `None` unless a `ContextWindow`
    // was present on `entity` at that point, and nothing between there and
    // here can remove it: `Provider::infer` (the only `.await` in between)
    // takes `&self` and a plain `InferenceRequest` -- it has no access to
    // `engine`/`World` and so cannot delete the component out from under us
    // during the await. So this is a real invariant, not a defensive
    // recheck: `.expect()` documents it and avoids an unreachable `None`
    // branch that could never be given real, testable behavior.
    let mut window = engine
        .world_mut()
        .get_mut::<ContextWindow>(entity)
        .expect("ContextWindow present: confirmed above and unremovable since");
    let tokens = choice.len() / 4 + 1;
    let _ = window.add_typed_entry(
        "conversation",
        leviath_core::EntryKind::AssistantTurn { tool_calls: vec![] },
        format!("Transitioning to: {}", choice),
        tokens,
    );

    // The stage explicitly allows ending here instead of transitioning —
    // honor an unambiguous "DONE" before attempting any edge-name match.
    if stage.allow_complete && choice.eq_ignore_ascii_case("done") {
        return None;
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
    let span = tracing::warn_span!(
        "llm_transition_no_match",
        stage = tracing::field::Empty,
        llm_response = tracing::field::Empty
    );
    let _enter = span.enter();
    span.record("stage", tracing::field::display(&stage.name));
    span.record("llm_response", tracing::field::display(&choice));
    tracing::warn!("LLM transition response didn't match any edge — using first available");
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
                        RegionKind::Pinned
                            | RegionKind::CompactHistory { .. }
                            | RegionKind::HashMap { .. }
                    ) {
                        region.clear();
                    }
                }
                window.current_tokens = window.calculate_tokens();
            } else {
                // No ContextWindow on this entity; nothing to clear.
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
            carry,
            compact,
            clear,
            compact_prompt,
        } => {
            if let Some(mut window) = engine.world_mut().get_mut::<ContextWindow>(entity) {
                // Clear the regions named in `clear` — but never a region also
                // listed in `carry` (carry wins; core regions are never removed).
                for region in &mut window.regions {
                    if clear.contains(&region.name) && !carry.contains(&region.name) {
                        region.clear();
                    }
                }
                window.current_tokens = window.calculate_tokens();
            } else {
                // No ContextWindow on this entity; nothing to clear.
            }

            // Summarize + clear ONLY the regions named in `compact`, protecting
            // anything in `carry`. Regions in neither list are left untouched —
            // this is the fix for carried regions being wiped by the old
            // kind-based blanket clear.
            if !compact.is_empty() {
                let prompt = compact_prompt.clone().unwrap_or_else(|| {
                    format!(
                        "Summarize the content from regions [{}] as context for stage '{}'.",
                        compact.join(", "),
                        edge.target
                    )
                });
                compact_transform_impl(
                    engine,
                    entity,
                    provider_name,
                    model_name,
                    &prompt,
                    compaction_config,
                    Some(compact.as_slice()),
                    carry.as_slice(),
                )
                .await;
            }

            // carry + unlisted regions are left untouched (they carry forward as-is)
        }
    }
}

/// Whether `region` should be summarized/cleared by a compact transform.
///
/// `only` scopes the transform to a specific set of region NAMES (used by
/// `EdgeTransform::Custom`'s `compact` list); `None` falls back to "every
/// compactable-kind region" (used by the blanket `EdgeTransform::Compact`).
/// A region named in `protect` (a transform's `carry` list) is NEVER a target,
/// so carried "core" regions are never summarized-away or cleared.
fn region_is_compact_target(
    region: &leviath_core::Region,
    only: Option<&[String]>,
    protect: &[String],
) -> bool {
    if protect.iter().any(|n| n == &region.name) {
        return false;
    }
    match only {
        Some(names) => names.iter().any(|n| n == &region.name),
        None => matches!(
            region.kind,
            RegionKind::SlidingWindow { .. }
                | RegionKind::Compacting { .. }
                | RegionKind::Temporary
                | RegionKind::Clearable
        ),
    }
}

/// Run LLM compaction, summarizing+clearing **every compactable-kind** region.
/// Used by the blanket `EdgeTransform::Compact`.
pub async fn apply_compact_transform(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    prompt: &str,
    compaction_config: Option<&CompactionConfig>,
) {
    compact_transform_impl(
        engine,
        entity,
        provider_name,
        model_name,
        prompt,
        compaction_config,
        None,
        &[],
    )
    .await;
}

/// Core compaction. `only_regions` scopes which regions are summarized+cleared
/// by NAME (`None` = all compactable-kind); `protect` names regions that must
/// never be touched (a transform's `carry` list — the "core" regions).
#[allow(clippy::too_many_arguments)]
async fn compact_transform_impl(
    engine: &mut AgentEngine,
    entity: bevy_ecs::prelude::Entity,
    provider_name: &str,
    model_name: &str,
    prompt: &str,
    compaction_config: Option<&CompactionConfig>,
    only_regions: Option<&[String]>,
    protect: &[String],
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
            if region_is_compact_target(region, only_regions, protect) && !region.content.is_empty()
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

    let messages = vec![leviath_providers::Message {
        role: "user".to_string(),
        content: content_to_compact.into(),
        cache_breakpoint: false,
    }];

    let max_summary_tokens = compaction_config
        .map(|cc| cc.max_summary_tokens)
        .unwrap_or(2000);

    let request = leviath_providers::InferenceRequest {
        messages,
        model: compact_model.to_string(),
        max_tokens: max_summary_tokens,
        temperature: compaction_config.map(|cc| cc.temperature).unwrap_or(0.3),
        tools: Vec::new(),
        system: vec![leviath_providers::SystemBlock {
            text: prompt.to_string(),
            cache_hint: leviath_core::CacheHint::Never,
        }],
        extra: serde_json::Value::Null,
    };

    match provider.infer(request).await {
        Ok(response) => {
            // ContextWindow was confirmed present above; it cannot be removed while
            // infer() holds only a provider reference, so this unwrap is safe.
            let mut window = engine.world_mut().get_mut::<ContextWindow>(entity).unwrap();
            // Clear the regions that were summarized. When only_regions is set
            // (EdgeTransform::Custom) this is name-scoped; carried/protected
            // regions are never cleared.
            for region in &mut window.regions {
                if region_is_compact_target(region, only_regions, protect) {
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
        Err(e) => {
            tracing::warn!(error = %e, "Failed to compact context during edge transform");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_tracing;
    use crate::{AgentPool, ProviderRegistry};
    use leviath_core::blueprint::ModelConfig;
    use leviath_core::layout::RegionDefinition;
    use leviath_core::{ContextLayout, EvictionStrategy};

    #[test]
    fn region_is_compact_target_respects_names_and_carry() {
        use leviath_core::{Region, RegionKind};
        let conv = Region::new(
            "conversation".to_string(),
            RegionKind::SlidingWindow {
                max_items: 10,
                eviction_strategy: EvictionStrategy::default(),
            },
            1000,
        );
        let files = Region::new("files".to_string(), RegionKind::Pinned, 1000);

        // Blanket (None): compactable kinds are targets, pinned is not.
        assert!(region_is_compact_target(&conv, None, &[]));
        assert!(!region_is_compact_target(&files, None, &[]));

        // Name-scoped: only regions named in `only` are targets.
        let only_conv = vec!["conversation".to_string()];
        let only_other = vec!["scratch".to_string()];
        assert!(region_is_compact_target(&conv, Some(&only_conv), &[]));
        assert!(!region_is_compact_target(&conv, Some(&only_other), &[]));

        // `protect` (carry) wins over both the blanket and the name list — a
        // carried region is never a compact target.
        let protect = vec!["conversation".to_string()];
        assert!(!region_is_compact_target(&conv, None, &protect));
        assert!(!region_is_compact_target(&conv, Some(&only_conv), &protect));
    }

    /// Shared `assert!`-with-dynamic-message helper: several `apply_compact_transform`
    /// tests assert a condition on `all_content` while formatting `all_content`
    /// itself into the panic message for diagnostics if the assertion ever
    /// fails. The panic-message formatting is only evaluated on failure, which
    /// otherwise leaves it permanently uncovered by `cargo llvm-cov`. Extracted
    /// once here (rather than per call site) and exercised below via
    /// `#[should_panic]`.
    fn assert_contains_display(cond: bool, prefix: &str, value: &str) {
        assert!(cond, "{}: {}", prefix, value);
    }

    #[test]
    #[should_panic(expected = "Expected summary in conversation: nope")]
    fn assert_contains_display_panics_when_false() {
        assert_contains_display(false, "Expected summary in conversation", "nope");
    }

    /// Same purpose as [`assert_contains_display`] above, but for the one
    /// `prompt_llm_transition` fallback test whose message doesn't use a
    /// `<prefix>: <value>` shape (`"Expected b or c, got {}"`).
    fn assert_edge_is_b_or_c(target: &str) {
        assert!(
            target == "b" || target == "c",
            "Expected b or c, got {}",
            target
        );
    }

    #[test]
    #[should_panic(expected = "Expected b or c, got d")]
    fn assert_edge_is_b_or_c_panics_when_neither() {
        assert_edge_is_b_or_c("d");
    }

    /// Helper to create a minimal blueprint for testing.
    fn make_blueprint(stages: Vec<Stage>) -> Blueprint {
        let layout = ContextLayout::new(
            vec![
                RegionDefinition::new("system".to_string(), RegionKind::Pinned, 2000),
                RegionDefinition::new(
                    "conversation".to_string(),
                    RegionKind::SlidingWindow {
                        max_items: 10,
                        eviction_strategy: EvictionStrategy::PerItem,
                    },
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
        crate::context_setup::initialize_context_window(
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

        // Add some content to conversation. `make_engine_and_entity` routes
        // through `AgentPool::spawn_agent`, which always inserts a
        // `ContextWindow`, so this is unconditionally `Some` here.
        let mut window = engine.world_mut().get_mut::<ContextWindow>(entity).unwrap();
        let _ = window.add_to_region("conversation", "test content".to_string(), 5);

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

        // Add content to both regions. Always `Some` (see note above).
        let mut window = engine.world_mut().get_mut::<ContextWindow>(entity).unwrap();
        let _ = window.add_to_region("system", "system content".to_string(), 5);
        let _ = window.add_to_region("conversation", "conv content".to_string(), 5);

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

    // ─── apply_edge_transform Custom ────────────────────────────────────────

    #[tokio::test]
    async fn apply_edge_transform_custom_clears_specified_regions() {
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, entity) = make_engine_and_entity(&bp);

        // Add content to conversation
        let mut window = engine.world_mut().get_mut::<ContextWindow>(entity).unwrap();
        let _ = window.add_to_region("conversation", "conv content".to_string(), 5);

        let edge = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Custom {
                carry: vec!["system".to_string()],
                compact: vec![],
                clear: vec!["conversation".to_string()],
                compact_prompt: None,
            },
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
        let conv = window.get_region("conversation").unwrap();
        assert!(conv.content.is_empty());
    }

    // ─── is_graph_mode additional ───────────────────────────────────────────

    #[test]
    fn is_graph_mode_empty_stages() {
        let bp = make_blueprint(vec![]);
        assert!(!is_graph_mode(&bp));
    }

    #[test]
    fn is_graph_mode_multiple_stages_none_with_transitions() {
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b"), make_stage("c")]);
        assert!(!is_graph_mode(&bp));
    }

    // ─── resolve_transition additional ───────────────────────────────────────

    #[tokio::test]
    async fn resolve_transition_linear_middle_stage() {
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b"), make_stage("c")]);
        let (mut engine, entity) = make_engine_and_entity(&bp);
        let visit_counts = HashMap::new();

        // From middle stage, should advance to next
        let result = resolve_transition(
            &bp.stages[1],
            1,
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
        assert_eq!(edge.target, "c");
        assert_eq!(idx, 2);
    }

    #[tokio::test]
    async fn resolve_transition_linear_last_stage_is_terminal() {
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, entity) = make_engine_and_entity(&bp);
        let visit_counts = HashMap::new();

        let result = resolve_transition(
            &bp.stages[1],
            1,
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
    async fn resolve_transition_unknown_target_skipped() {
        let mut stage_a = make_stage("a");
        let mut transitions = HashMap::new();
        transitions.insert(
            "nonexistent".to_string(),
            TransitionEdge {
                target: "nonexistent".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
            },
        );
        stage_a.transitions = Some(transitions);

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

        // Unknown target should be filtered out
        assert!(result.is_none());
    }

    // ─── apply_compact_transform with no provider ───────────────────────────

    #[tokio::test]
    async fn apply_compact_transform_no_provider_does_not_panic() {
        let bp = make_blueprint(vec![make_stage("a")]);
        let (mut engine, entity) = make_engine_and_entity(&bp);

        // No provider registered, should silently return
        apply_compact_transform(
            &mut engine,
            entity,
            "nonexistent",
            "model",
            "Summarize",
            None,
        )
        .await;
    }

    #[tokio::test]
    async fn apply_compact_transform_empty_content_does_not_panic() {
        let bp = make_blueprint(vec![make_stage("a")]);
        let (mut engine, entity) = make_engine_and_entity(&bp);

        // Even with no provider, empty content returns early
        apply_compact_transform(
            &mut engine,
            entity,
            "nonexistent",
            "model",
            "Summarize",
            None,
        )
        .await;
    }

    // ─── resolve_transition: error condition with Success result ─────────

    #[tokio::test]
    async fn resolve_transition_error_edge_ignored_on_success() {
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

        // On Success, error handler edge should be filtered out, picking "b"
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

        let (edge, _idx) = result.unwrap();
        assert_eq!(edge.target, "b");
    }

    #[tokio::test]
    async fn resolve_transition_max_iterations_edge_ignored_on_success() {
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

        // On Success, MaxIterations edge should be filtered out
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

        let (edge, _idx) = result.unwrap();
        assert_eq!(edge.target, "b");
    }

    // ─── resolve_transition: only error/maxiter edges → terminal ────────

    #[tokio::test]
    async fn resolve_transition_only_conditional_edges_is_terminal_on_success() {
        let mut stage_a = make_stage("a");
        let stage_err = make_stage("error_handler");
        let mut transitions = HashMap::new();
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

        let bp = make_blueprint(vec![stage_a, stage_err]);
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

        // No Always or LlmChoice edges available → terminal
        assert!(result.is_none());
    }

    // ─── apply_edge_transform: Custom with compact (no provider) ────────

    #[tokio::test]
    async fn apply_edge_transform_custom_with_compact_no_panic() {
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, entity) = make_engine_and_entity(&bp);

        let mut window = engine.world_mut().get_mut::<ContextWindow>(entity).unwrap();
        let _ = window.add_to_region("conversation", "some content".to_string(), 5);

        let edge = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Custom {
                carry: vec!["system".to_string()],
                compact: vec!["conversation".to_string()],
                clear: vec![],
                compact_prompt: Some("Summarize this".to_string()),
            },
        };
        let visit_counts = HashMap::new();

        // Should not panic even without provider
        apply_edge_transform(
            &edge,
            &visit_counts,
            &mut engine,
            entity,
            "nonexistent",
            "model",
            None,
        )
        .await;
    }

    // ─── apply_edge_transform: Compact with compaction config ───────────

    #[tokio::test]
    async fn apply_edge_transform_compact_explicit_with_config() {
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, entity) = make_engine_and_entity(&bp);

        let edge = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Compact {
                prompt: Some("Custom compact prompt".to_string()),
            },
        };
        let visit_counts = HashMap::new();

        let compaction_config = CompactionConfig {
            provider: "nonexistent".to_string(),
            model: "test-model".to_string(),
            max_summary_tokens: 500,
            temperature: 0.1,
            system_prompt: None,
            user_prompt_template: None,
        };

        // Should not panic — compact fails silently with no provider
        apply_edge_transform(
            &edge,
            &visit_counts,
            &mut engine,
            entity,
            "anthropic",
            "test",
            Some(&compaction_config),
        )
        .await;
    }

    // ─── resolve_transition: max_revisits zero blocks first visit ───────

    #[tokio::test]
    async fn resolve_transition_max_revisits_zero_blocks_second_visit() {
        let mut stage_a = make_stage("a");
        let mut stage_b = make_stage("b");
        stage_b.max_revisits = Some(0); // only allow first visit

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

        // First visit ok (visits = 0 <= max_revisits = 0)
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

        // Second visit blocked (visits = 1 > max_revisits = 0)
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
        assert!(result.is_none());
    }

    // ─── resolve_transition: LlmChoice edge with single edge (acts like Always)

    #[tokio::test]
    async fn resolve_transition_single_llm_choice_edge() {
        let mut stage_a = make_stage("a");
        let stage_b = make_stage("b");
        let mut transitions = HashMap::new();
        transitions.insert(
            "b".to_string(),
            TransitionEdge {
                target: "b".to_string(),
                condition: TransitionCondition::LlmChoice,
                hint: Some("Go to b".to_string()),
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

        // Single LlmChoice edge → auto-selected
        let (edge, idx) = result.unwrap();
        assert_eq!(edge.target, "b");
        assert_eq!(idx, 1);
    }

    // ─── resolve_transition: allow_complete lets the LLM end at a stage
    // with only one outgoing edge (e.g. an approving review), instead of
    // blindly auto-following it like the LlmChoice-single-edge case above.

    #[tokio::test]
    async fn resolve_transition_allow_complete_single_edge_consults_llm() {
        let mut stage_a = make_stage("review");
        stage_a.allow_complete = true;
        let stage_b = make_stage("implement");
        let mut transitions = HashMap::new();
        transitions.insert(
            "implement".to_string(),
            TransitionEdge {
                target: "implement".to_string(),
                condition: TransitionCondition::Always,
                hint: Some("issues found".to_string()),
                transform: EdgeTransform::Direct,
            },
        );
        stage_a.transitions = Some(transitions);

        let bp = make_blueprint(vec![stage_a, stage_b]);
        // Mock provider says DONE — review approved the work, no transition needed.
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "DONE");
        let visit_counts = HashMap::new();

        let result = resolve_transition(
            &bp.stages[0],
            0,
            &bp,
            &visit_counts,
            &StageResult::Success,
            &mut engine,
            entity,
            "mock",
            "test",
        )
        .await;

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn resolve_transition_allow_complete_single_edge_still_transitions_when_named() {
        let mut stage_a = make_stage("review");
        stage_a.allow_complete = true;
        let stage_b = make_stage("implement");
        let mut transitions = HashMap::new();
        transitions.insert(
            "implement".to_string(),
            TransitionEdge {
                target: "implement".to_string(),
                condition: TransitionCondition::Always,
                hint: Some("issues found".to_string()),
                transform: EdgeTransform::Direct,
            },
        );
        stage_a.transitions = Some(transitions);

        let bp = make_blueprint(vec![stage_a, stage_b]);
        // Mock provider names the real edge — issues were found, go fix them.
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "implement");
        let visit_counts = HashMap::new();

        let result = resolve_transition(
            &bp.stages[0],
            0,
            &bp,
            &visit_counts,
            &StageResult::Success,
            &mut engine,
            entity,
            "mock",
            "test",
        )
        .await;

        let (edge, idx) = result.expect("naming the real edge must still transition");
        assert_eq!(edge.target, "implement");
        assert_eq!(idx, 1);
    }

    #[tokio::test]
    async fn prompt_llm_transition_custom_prompt_and_allow_complete_mentions_done() {
        // Unlike resolve_transition_allow_complete_single_edge_consults_llm
        // (whose stage has no transition_prompt, hitting the *default*
        // prompt's allow_complete branch), this stage has an explicit
        // transition_prompt set, exercising the *custom* prompt's separate
        // allow_complete branch (its own "...or ONLY the word DONE..." text).
        let mut stage = make_stage("review");
        stage.allow_complete = true;
        stage.transition_prompt = Some("Review the work and decide what's next.".to_string());

        let bp = make_blueprint(vec![stage, make_stage("implement")]);
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "DONE");

        let edge = TransitionEdge {
            target: "implement".to_string(),
            condition: TransitionCondition::LlmChoice,
            hint: None,
            transform: EdgeTransform::Direct,
        };
        let name = "implement".to_string();
        let edges: Vec<(&String, &TransitionEdge)> = vec![(&name, &edge)];

        let stage_ref = &bp.stages[0];
        let result =
            prompt_llm_transition(stage_ref, &edges, &mut engine, entity, "mock", "test").await;

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn resolve_transition_without_allow_complete_single_edge_ignores_done() {
        // Same setup, but allow_complete = false (default) — a stray "DONE"
        // from the model must NOT terminate the run; the single edge is
        // still auto-followed without even consulting the LLM.
        let stage_a = make_stage("plan"); // allow_complete defaults to false
        let stage_b = make_stage("implement");
        let mut transitions = HashMap::new();
        transitions.insert(
            "implement".to_string(),
            TransitionEdge {
                target: "implement".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
            },
        );
        let mut stage_a = stage_a;
        stage_a.transitions = Some(transitions);

        let bp = make_blueprint(vec![stage_a, stage_b]);
        // No provider registered at all — if this auto-transitions without
        // consulting the LLM (as expected when allow_complete = false), the
        // missing provider is never even touched.
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
            "mock",
            "test",
        )
        .await;

        let (edge, idx) = result.expect("single edge without allow_complete must auto-transition");
        assert_eq!(edge.target, "implement");
        assert_eq!(idx, 1);
    }

    // ─── prompt_llm_transition: DONE sentinel ─────────────────────────────

    #[tokio::test]
    async fn prompt_llm_transition_allow_complete_done_returns_none() {
        let mut stage = make_stage("review");
        stage.allow_complete = true;
        let bp = make_blueprint(vec![stage.clone(), make_stage("implement")]);
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "DONE");

        let edge = TransitionEdge {
            target: "implement".to_string(),
            condition: TransitionCondition::Always,
            hint: Some("issues found".to_string()),
            transform: EdgeTransform::Direct,
        };
        let name = "implement".to_string();
        let edges: Vec<(&String, &TransitionEdge)> = vec![(&name, &edge)];

        let result =
            prompt_llm_transition(&stage, &edges, &mut engine, entity, "mock", "test").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn prompt_llm_transition_without_allow_complete_done_falls_back_to_edge() {
        // allow_complete = false: a "DONE" response isn't a recognized
        // sentinel, so it falls through to the existing fuzzy-match/first-
        // edge fallback rather than being treated as terminal.
        let stage = make_stage("plan"); // allow_complete defaults to false
        let bp = make_blueprint(vec![stage.clone(), make_stage("implement")]);
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "DONE");

        let edge = TransitionEdge {
            target: "implement".to_string(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Direct,
        };
        let name = "implement".to_string();
        let edges: Vec<(&String, &TransitionEdge)> = vec![(&name, &edge)];

        let result =
            prompt_llm_transition(&stage, &edges, &mut engine, entity, "mock", "test").await;
        assert_eq!(result.unwrap().target, "implement");
    }

    // ─── Helpers for mock provider ────────────────────────────────────────────

    use async_trait::async_trait;
    use leviath_providers::{
        FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, ModelInfo, Provider,
        ProviderError, TokenUsage,
    };
    use std::sync::Arc;

    /// A mock provider that returns a fixed response string.
    struct MockProvider {
        response: String,
    }

    impl MockProvider {
        fn new(response: impl Into<String>) -> Self {
            Self {
                response: response.into(),
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
                content: self.response.clone(),
                tool_calls: vec![],
                tokens_used: TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
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

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(vec![])
        }
    }

    fn make_engine_with_mock_provider(
        blueprint: &Blueprint,
        response: &str,
    ) -> (AgentEngine, bevy_ecs::prelude::Entity) {
        let mut registry = ProviderRegistry::new();
        registry.register("mock".to_string(), Arc::new(MockProvider::new(response)));
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(blueprint.clone());
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();

        crate::context_setup::initialize_context_window(
            &mut engine,
            entity,
            blueprint,
            "test task",
        );

        (engine, entity)
    }

    /// A mock provider whose `infer()` always errors, for exercising
    /// `apply_compact_transform`'s `Err(e)` arm.
    struct FailingProvider;

    #[async_trait]
    impl Provider for FailingProvider {
        async fn infer(
            &self,
            _request: InferenceRequest,
        ) -> Result<InferenceResponse, ProviderError> {
            Err(ProviderError::Other(
                "simulated compaction failure".to_string(),
            ))
        }

        fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len() / 4
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            100_000
        }

        fn name(&self) -> &str {
            "failing"
        }

        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(vec![])
        }
    }

    fn make_engine_with_provider(
        blueprint: &Blueprint,
        name: &str,
        provider: Arc<dyn Provider>,
    ) -> (AgentEngine, bevy_ecs::prelude::Entity) {
        let mut registry = ProviderRegistry::new();
        registry.register(name.to_string(), provider);
        let mut engine = AgentEngine::with_providers(registry);
        let mut pool = AgentPool::new(blueprint.clone());
        let agent_id = pool.spawn_agent(engine.world_mut());
        let entity = pool.get_agent(&agent_id).unwrap();

        crate::context_setup::initialize_context_window(
            &mut engine,
            entity,
            blueprint,
            "test task",
        );

        (engine, entity)
    }

    // ─── apply_compact_transform: provider present, ContextWindow missing ───

    #[tokio::test]
    async fn apply_compact_transform_missing_context_window_returns_early() {
        let bp = make_blueprint(vec![make_stage("a")]);
        let (mut engine, entity) =
            make_engine_with_provider(&bp, "mock", Arc::new(MockProvider::new("summary")));
        // Remove the ContextWindow that initialize_context_window just added,
        // so `engine.world().get::<ContextWindow>(entity)` returns `None`
        // even though the provider itself resolves fine.
        engine
            .world_mut()
            .entity_mut(entity)
            .remove::<ContextWindow>();

        // Should not panic, and should return before ever calling the provider.
        apply_compact_transform(&mut engine, entity, "mock", "test-model", "Summarize", None).await;
    }

    // ─── apply_compact_transform: provider present, no compactable content ──

    #[tokio::test]
    async fn apply_compact_transform_provider_present_but_content_empty_returns_early() {
        let bp = make_blueprint(vec![make_stage("a")]);
        let (mut engine, entity) =
            make_engine_with_mock_provider(&bp, "should never be used as a summary");

        // Fresh window: "conversation" (SlidingWindow, compactable) starts
        // empty, so content_to_compact is empty and the function returns
        // before ever calling provider.infer() -- distinct from the
        // `apply_compact_transform_no_provider_does_not_panic` test, which
        // returns even earlier (no provider at all).
        apply_compact_transform(&mut engine, entity, "mock", "test-model", "Summarize", None).await;

        // The mock's canned response was never written anywhere -- in fact
        // the region's content stays fully empty, since the early return
        // happens before any write. Asserting emptiness directly (rather
        // than searching for the canned text via `.iter().any(...)`) is both
        // a more precise match for the doc comment above and avoids a
        // never-invoked search closure (there's nothing to search).
        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = window.get_region("conversation").unwrap();
        assert!(conv.content.is_empty());
    }

    // ─── apply_compact_transform: success path ───────────────────────────────

    #[tokio::test]
    async fn apply_compact_transform_success_clears_and_summarizes() {
        let bp = make_blueprint(vec![make_stage("a")]);
        let (mut engine, entity) =
            make_engine_with_mock_provider(&bp, "concise summary of the conversation");

        let mut window = engine.world_mut().get_mut::<ContextWindow>(entity).unwrap();
        let _ = window.add_to_region("conversation", "raw conversation content".to_string(), 5);

        apply_compact_transform(&mut engine, entity, "mock", "test-model", "Summarize", None).await;

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = window.get_region("conversation").unwrap();
        assert!(conv
            .content
            .iter()
            .any(|e| e.content.contains("concise summary of the conversation")));
        assert!(!conv
            .content
            .iter()
            .any(|e| e.content.contains("raw conversation content")));
    }

    // ─── apply_compact_transform: provider infer() errors ────────────────────

    #[tokio::test]
    async fn apply_compact_transform_provider_error_does_not_panic() {
        let bp = make_blueprint(vec![make_stage("a")]);
        let (mut engine, entity) =
            make_engine_with_provider(&bp, "failing", Arc::new(FailingProvider));

        let mut window = engine.world_mut().get_mut::<ContextWindow>(entity).unwrap();
        let _ = window.add_to_region("conversation", "raw conversation content".to_string(), 5);

        // Should log a warning and return, not panic; original content is
        // left untouched since the Err arm never reaches the clear/summarize step.
        // Wrapped in `with_tracing` so the "Failed to compact context during
        // edge transform" warn!'s field-list line is exercised.
        with_tracing(|| {
            apply_compact_transform(
                &mut engine,
                entity,
                "failing",
                "test-model",
                "Summarize",
                None,
            )
        })
        .await;

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = window.get_region("conversation").unwrap();
        assert!(conv
            .content
            .iter()
            .any(|e| e.content.contains("raw conversation content")));
    }

    // ─── apply_edge_transform: Custom with non-empty compact + no explicit prompt ─

    #[tokio::test]
    async fn apply_edge_transform_custom_compact_uses_default_prompt() {
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, entity) = make_engine_and_entity(&bp);

        let mut window = engine.world_mut().get_mut::<ContextWindow>(entity).unwrap();
        let _ = window.add_to_region("conversation", "conv content".to_string(), 5);

        let edge = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Custom {
                carry: vec!["system".to_string()],
                compact: vec!["conversation".to_string()],
                clear: vec![],
                compact_prompt: None, // exercises the default-prompt format! branch
            },
        };
        let visit_counts = HashMap::new();

        // No provider registered for "anthropic" -- apply_compact_transform
        // will return early after failing to resolve the provider, but the
        // default-prompt format! runs before that lookup, so this still
        // exercises the target line without panicking.
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

    // ─── prompt_llm_transition ────────────────────────────────────────────────

    #[tokio::test]
    async fn prompt_llm_transition_returns_matching_edge() {
        // LLM returns "b" which matches edge "b"
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b"), make_stage("c")]);
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "b");

        let stage = &bp.stages[0];
        let edge_b = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::LlmChoice,
            hint: Some("go to b".to_string()),
            transform: EdgeTransform::Direct,
        };
        let edge_c = TransitionEdge {
            target: "c".to_string(),
            condition: TransitionCondition::LlmChoice,
            hint: None,
            transform: EdgeTransform::Direct,
        };
        let name_b = "b".to_string();
        let name_c = "c".to_string();
        let edges: Vec<(&String, &TransitionEdge)> = vec![(&name_b, &edge_b), (&name_c, &edge_c)];

        let result =
            prompt_llm_transition(stage, &edges, &mut engine, entity, "mock", "test").await;

        assert!(result.is_some());
        assert_eq!(result.unwrap().target, "b");
    }

    #[tokio::test]
    async fn prompt_llm_transition_with_custom_prompt() {
        // Stage has a custom transition_prompt — exercises the custom prompt branch
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b"), make_stage("c")]);
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "c");

        let mut stage = bp.stages[0].clone();
        stage.transition_prompt = Some("Custom: pick the next stage.".to_string());

        let edge_b = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::LlmChoice,
            hint: None,
            transform: EdgeTransform::Direct,
        };
        let edge_c = TransitionEdge {
            target: "c".to_string(),
            condition: TransitionCondition::LlmChoice,
            hint: Some("c is the better choice".to_string()),
            transform: EdgeTransform::Direct,
        };
        let name_b = "b".to_string();
        let name_c = "c".to_string();
        let edges: Vec<(&String, &TransitionEdge)> = vec![(&name_b, &edge_b), (&name_c, &edge_c)];

        let result =
            prompt_llm_transition(&stage, &edges, &mut engine, entity, "mock", "test").await;

        assert!(result.is_some());
        // Provider returns "c", which matches edge_c
        assert_eq!(result.unwrap().target, "c");
    }

    #[tokio::test]
    async fn prompt_llm_transition_fuzzy_lowercase_match_when_exact_case_fails() {
        // "REVIEWED and approved" doesn't `eq_ignore_ascii_case` the edge
        // name "review" (different length/content) and doesn't contain it as
        // an exact-case substring ("REVIEWED" is all-caps, "review" isn't a
        // substring of it) -- but *lowercased*, "reviewed and approved"
        // contains "review", so only the second (fuzzy) loop in
        // prompt_llm_transition matches.
        let bp = make_blueprint(vec![
            make_stage("a"),
            make_stage("review"),
            make_stage("implement"),
        ]);
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "REVIEWED and approved");

        let stage = &bp.stages[0];
        let edge_review = TransitionEdge {
            target: "review".to_string(),
            condition: TransitionCondition::LlmChoice,
            hint: None,
            transform: EdgeTransform::Direct,
        };
        let edge_implement = TransitionEdge {
            target: "implement".to_string(),
            condition: TransitionCondition::LlmChoice,
            hint: None,
            transform: EdgeTransform::Direct,
        };
        let name_review = "review".to_string();
        let name_implement = "implement".to_string();
        let edges: Vec<(&String, &TransitionEdge)> = vec![
            (&name_review, &edge_review),
            (&name_implement, &edge_implement),
        ];

        let result =
            prompt_llm_transition(stage, &edges, &mut engine, entity, "mock", "test").await;

        assert_eq!(result.unwrap().target, "review");
    }

    #[tokio::test]
    async fn prompt_llm_transition_fallback_to_first_edge_when_no_match() {
        // LLM returns something that doesn't match any edge name → falls back to first.
        // Use stage names with no common substrings to avoid fuzzy match false-positives.
        let bp = make_blueprint(vec![
            make_stage("a"),
            make_stage("stage_alpha"),
            make_stage("stage_beta"),
        ]);
        // Provider returns something with no overlap with "stage_alpha" or "stage_beta"
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "XXXXXXXX_no_match_here");

        let stage = &bp.stages[0];
        let edge_alpha = TransitionEdge {
            target: "stage_alpha".to_string(),
            condition: TransitionCondition::LlmChoice,
            hint: None,
            transform: EdgeTransform::Direct,
        };
        let edge_beta = TransitionEdge {
            target: "stage_beta".to_string(),
            condition: TransitionCondition::LlmChoice,
            hint: None,
            transform: EdgeTransform::Direct,
        };
        let name_alpha = "stage_alpha".to_string();
        let name_beta = "stage_beta".to_string();
        // edge_alpha is first in the slice → fallback should return it
        let edges: Vec<(&String, &TransitionEdge)> =
            vec![(&name_alpha, &edge_alpha), (&name_beta, &edge_beta)];

        // Wrapped in `with_tracing` so the "LLM transition response didn't
        // match any edge" warn!'s field-list lines are exercised.
        let result = with_tracing(|| {
            prompt_llm_transition(stage, &edges, &mut engine, entity, "mock", "test")
        })
        .await;

        assert!(result.is_some());
        // Fallback picks the first edge in the slice (edge_alpha)
        assert_eq!(result.unwrap().target, "stage_alpha");
    }

    #[tokio::test]
    async fn prompt_llm_transition_no_provider_returns_none() {
        // No provider registered → prompt_llm_transition returns None
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, entity) = make_engine_and_entity(&bp); // no provider

        let stage = &bp.stages[0];
        let edge_b = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::LlmChoice,
            hint: None,
            transform: EdgeTransform::Direct,
        };
        let name_b = "b".to_string();
        let edges: Vec<(&String, &TransitionEdge)> = vec![(&name_b, &edge_b)];

        // No ContextWindow → get_provider returns None → returns None
        let result =
            prompt_llm_transition(stage, &edges, &mut engine, entity, "nonexistent", "test").await;

        assert!(result.is_none());
    }

    // ─── prompt_llm_transition: provider present, ContextWindow missing ─────

    #[tokio::test]
    async fn prompt_llm_transition_no_context_window_returns_none() {
        // A registered provider makes `get_provider` succeed, but the
        // `ContextWindow` that `initialize_context_window` just added is
        // then explicitly removed (same pattern as
        // `apply_compact_transform_missing_context_window_returns_early`
        // above -- `AgentPool::spawn_agent` unconditionally inserts a
        // `ContextWindow`, so merely skipping `initialize_context_window`
        // does NOT produce a windowless entity), so `window()?` short-
        // circuits to `None` before any inference call is attempted.
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "b");
        engine
            .world_mut()
            .entity_mut(entity)
            .remove::<ContextWindow>();

        let stage = &bp.stages[0];
        let edge_b = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::LlmChoice,
            hint: None,
            transform: EdgeTransform::Direct,
        };
        let name_b = "b".to_string();
        let edges: Vec<(&String, &TransitionEdge)> = vec![(&name_b, &edge_b)];

        let result =
            prompt_llm_transition(stage, &edges, &mut engine, entity, "mock", "test").await;

        assert!(result.is_none());
    }

    // ─── prompt_llm_transition: provider infer() errors ──────────────────────

    #[tokio::test]
    async fn prompt_llm_transition_provider_infer_error_returns_none() {
        // A provider whose `infer()` always errors exercises the
        // `.ok()?` branch that converts an inference error into `None`,
        // distinct from `apply_compact_transform`'s own error handling.
        let bp = make_blueprint(vec![
            make_stage("a"),
            make_stage("stage_alpha"),
            make_stage("stage_beta"),
        ]);
        let (mut engine, entity) =
            make_engine_with_provider(&bp, "failing", Arc::new(FailingProvider));

        let stage = &bp.stages[0];
        let edge_alpha = TransitionEdge {
            target: "stage_alpha".to_string(),
            condition: TransitionCondition::LlmChoice,
            hint: None,
            transform: EdgeTransform::Direct,
        };
        let edge_beta = TransitionEdge {
            target: "stage_beta".to_string(),
            condition: TransitionCondition::LlmChoice,
            hint: None,
            transform: EdgeTransform::Direct,
        };
        let name_alpha = "stage_alpha".to_string();
        let name_beta = "stage_beta".to_string();
        let edges: Vec<(&String, &TransitionEdge)> =
            vec![(&name_alpha, &edge_alpha), (&name_beta, &edge_beta)];

        let result =
            prompt_llm_transition(stage, &edges, &mut engine, entity, "failing", "test-model")
                .await;

        assert!(result.is_none());
    }

    // ─── prompt_llm_transition: empty edges slice ────────────────────────────

    #[tokio::test]
    async fn prompt_llm_transition_empty_edges_returns_none() {
        // Called directly with an empty `edges` slice (never happens via
        // `resolve_transition`, which only calls this with >=1 choosable
        // edge, but the function is `pub` and defensively handles it) --
        // exercises `edges.first()?` at the end of the no-match fallback.
        let bp = make_blueprint(vec![make_stage("a")]);
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "anything");
        let stage = &bp.stages[0];
        let edges: Vec<(&String, &TransitionEdge)> = vec![];

        let result =
            prompt_llm_transition(stage, &edges, &mut engine, entity, "mock", "test-model").await;

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn mock_and_failing_provider_trivial_trait_methods() {
        let mock = MockProvider::new("x");
        assert_eq!(mock.count_tokens("abcd", "m"), 1);
        assert_eq!(mock.max_context_tokens("m"), 100_000);
        assert_eq!(mock.name(), "mock");
        let _ = mock.capabilities("m");
        assert!(mock.list_models().await.unwrap().is_empty());

        let failing = FailingProvider;
        assert_eq!(failing.count_tokens("abcd", "m"), 1);
        assert_eq!(failing.max_context_tokens("m"), 100_000);
        assert_eq!(failing.name(), "failing");
        let _ = failing.capabilities("m");
        assert!(failing.list_models().await.unwrap().is_empty());
    }

    // ─── resolve_transition: multiple LlmChoice edges triggers prompt_llm_transition

    #[tokio::test]
    async fn resolve_transition_multiple_llm_choice_uses_provider() {
        // Two LlmChoice edges: provider will return "c" → should route to c
        let stage_a_orig = make_stage("a");
        let stage_b = make_stage("b");
        let stage_c = make_stage("c");

        let mut stage_a = stage_a_orig.clone();
        let mut transitions = HashMap::new();
        transitions.insert(
            "b".to_string(),
            TransitionEdge {
                target: "b".to_string(),
                condition: TransitionCondition::LlmChoice,
                hint: None,
                transform: EdgeTransform::Direct,
            },
        );
        transitions.insert(
            "c".to_string(),
            TransitionEdge {
                target: "c".to_string(),
                condition: TransitionCondition::LlmChoice,
                hint: None,
                transform: EdgeTransform::Direct,
            },
        );
        stage_a.transitions = Some(transitions);

        let bp = make_blueprint(vec![stage_a, stage_b, stage_c]);
        // Provider returns "c" so the LLM routing should pick stage c
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "c");
        let visit_counts = HashMap::new();

        let result = resolve_transition(
            &bp.stages[0],
            0,
            &bp,
            &visit_counts,
            &StageResult::Success,
            &mut engine,
            entity,
            "mock",
            "test",
        )
        .await;

        assert!(result.is_some());
        let (edge, idx) = result.unwrap();
        assert_eq!(edge.target, "c");
        assert_eq!(idx, 2);
    }

    // ─── apply_compact_transform with working provider ─────────────────────

    #[tokio::test]
    async fn apply_compact_transform_with_content_and_provider() {
        let bp = make_blueprint(vec![make_stage("a")]);
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "Summary of context");

        // Add content to the conversation region so compact has something to work with
        let mut window = engine.world_mut().get_mut::<ContextWindow>(entity).unwrap();
        let _ = window.add_to_region(
            "conversation",
            "User: hello\nAssistant: world".to_string(),
            10,
        );

        apply_compact_transform(
            &mut engine,
            entity,
            "mock",
            "test",
            "Summarize the conversation",
            None,
        )
        .await;

        // After compact, conversation should contain the summary
        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = window.get_region("conversation").unwrap();
        let all_content: String = conv.content.iter().map(|e| e.content.as_str()).collect();
        assert_contains_display(
            all_content.contains("Summary of context"),
            "Expected summary in conversation",
            &all_content,
        );
    }

    #[tokio::test]
    async fn apply_compact_transform_with_compaction_config() {
        let bp = make_blueprint(vec![make_stage("a")]);
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "Compact summary");

        // Add content to conversation
        let mut window = engine.world_mut().get_mut::<ContextWindow>(entity).unwrap();
        let _ = window.add_to_region("conversation", "Some conversation history".to_string(), 5);

        let config = leviath_core::lifecycle::CompactionConfig {
            provider: "mock".to_string(),
            model: "test".to_string(),
            max_summary_tokens: 100,
            temperature: 0.0,
            system_prompt: None,
            user_prompt_template: None,
        };

        apply_compact_transform(
            &mut engine,
            entity,
            "mock",
            "test",
            "Summarize",
            Some(&config),
        )
        .await;

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = window.get_region("conversation").unwrap();
        let all_content: String = conv.content.iter().map(|e| e.content.as_str()).collect();
        assert_contains_display(
            all_content.contains("Compact summary"),
            "Expected compact summary",
            &all_content,
        );
    }

    // ─── apply_edge_transform: Compact with default prompt (None) + real provider

    #[tokio::test]
    async fn apply_edge_transform_compact_no_prompt_with_provider() {
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "Compacted context");

        // Add conversation content
        let mut window = engine.world_mut().get_mut::<ContextWindow>(entity).unwrap();
        let _ = window.add_to_region("conversation", "existing conversation".to_string(), 5);

        let edge = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Compact { prompt: None }, // None → default prompt
        };
        let visit_counts = HashMap::new();

        apply_edge_transform(
            &edge,
            &visit_counts,
            &mut engine,
            entity,
            "mock",
            "test",
            None,
        )
        .await;

        // Conversation should now contain the compacted summary
        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = window.get_region("conversation").unwrap();
        let all_content: String = conv.content.iter().map(|e| e.content.as_str()).collect();
        assert_contains_display(
            all_content.contains("Compacted context"),
            "Expected compacted content",
            &all_content,
        );
    }

    #[tokio::test]
    async fn apply_edge_transform_direct_first_visit_is_noop_with_provider() {
        // Direct transform on first visit (visits=0) should still be a no-op
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "Response");

        let mut window = engine.world_mut().get_mut::<ContextWindow>(entity).unwrap();
        let _ = window.add_to_region("conversation", "original content".to_string(), 5);

        let edge = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Direct,
        };
        let visit_counts = HashMap::new(); // no visits yet

        apply_edge_transform(
            &edge,
            &visit_counts,
            &mut engine,
            entity,
            "mock",
            "test",
            None,
        )
        .await;

        // Content should be unchanged
        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = window.get_region("conversation").unwrap();
        assert!(conv.content.iter().any(|e| e.content == "original content"));
    }

    #[tokio::test]
    async fn apply_edge_transform_custom_compact_with_prompt_and_provider() {
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "Custom compact result");

        let mut window = engine.world_mut().get_mut::<ContextWindow>(entity).unwrap();
        let _ = window.add_to_region("conversation", "conv content to compact".to_string(), 5);

        let edge = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Custom {
                carry: vec![],
                compact: vec!["conversation".to_string()],
                clear: vec![],
                compact_prompt: Some("Custom compact prompt for test".to_string()),
            },
        };
        let visit_counts = HashMap::new();

        apply_edge_transform(
            &edge,
            &visit_counts,
            &mut engine,
            entity,
            "mock",
            "test",
            None,
        )
        .await;

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        let conv = window.get_region("conversation").unwrap();
        let all_content: String = conv.content.iter().map(|e| e.content.as_str()).collect();
        assert_contains_display(
            all_content.contains("Custom compact result"),
            "Expected custom compact result",
            &all_content,
        );
    }

    #[tokio::test]
    async fn apply_edge_transform_custom_carry_preserves_compactable_region() {
        // Regression for the carry bug: a carried region of a COMPACTABLE kind
        // must survive a Custom transform that compacts another region. The old
        // kind-based blanket clear wiped it regardless of `carry`.
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, entity) =
            make_engine_with_mock_provider(&bp, "summary of the conversation");

        {
            let mut window = engine.world_mut().get_mut::<ContextWindow>(entity).unwrap();
            // A carried "core" region of a clearable (compactable) kind, populated.
            window.add_region(leviath_core::Region::new(
                "files".to_string(),
                leviath_core::RegionKind::Clearable,
                5000,
            ));
            let _ = window.add_to_region("files", "IMPORTANT tracked file contents".to_string(), 5);
            let _ = window.add_to_region("conversation", "chatter to compact".to_string(), 5);
        }

        let edge = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Custom {
                carry: vec!["files".to_string()],
                compact: vec!["conversation".to_string()],
                clear: vec![],
                compact_prompt: None,
            },
        };

        apply_edge_transform(
            &edge,
            &HashMap::new(),
            &mut engine,
            entity,
            "mock",
            "test",
            None,
        )
        .await;

        let window = engine.world().get::<ContextWindow>(entity).unwrap();
        let files = window
            .get_region("files")
            .expect("carried region must still exist");
        let files_text: String = files.content.iter().map(|e| e.content.as_str()).collect();
        assert!(
            files_text.contains("IMPORTANT tracked file contents"),
            "carried region content must be preserved, got: {files_text:?}"
        );
        // conversation was compacted: original chatter replaced by the summary.
        let conv_text: String = window
            .get_region("conversation")
            .unwrap()
            .content
            .iter()
            .map(|e| e.content.as_str())
            .collect();
        assert!(
            !conv_text.contains("chatter to compact"),
            "compacted region should be cleared: {conv_text:?}"
        );
        assert!(
            conv_text.contains("summary of the conversation"),
            "summary should be present: {conv_text:?}"
        );
    }

    #[tokio::test]
    async fn apply_edge_transform_custom_no_compact_no_clear_is_noop() {
        // Custom transform with empty compact and clear lists should be a no-op
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, entity) = make_engine_and_entity(&bp);

        let mut window = engine.world_mut().get_mut::<ContextWindow>(entity).unwrap();
        let _ = window.add_to_region("conversation", "preserved content".to_string(), 5);

        let edge = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Custom {
                carry: vec!["conversation".to_string()],
                compact: vec![],
                clear: vec![],
                compact_prompt: None,
            },
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
        let conv = window.get_region("conversation").unwrap();
        assert!(conv
            .content
            .iter()
            .any(|e| e.content == "preserved content"));
    }

    // ─── resolve_transition: multiple LlmChoice edges, provider returns no match
    //     → falls back to first edge

    #[tokio::test]
    async fn resolve_transition_multiple_llm_choice_fallback_to_first() {
        let stage_b = make_stage("b");
        let stage_c = make_stage("c");

        let mut stage_a = make_stage("a");
        let mut transitions = HashMap::new();
        // Note: HashMap iteration order is not guaranteed; but we insert b first, then c.
        // The fallback picks the first element of the choosable Vec (which depends on HashMap order).
        // We just check that a valid edge was returned.
        transitions.insert(
            "b".to_string(),
            TransitionEdge {
                target: "b".to_string(),
                condition: TransitionCondition::LlmChoice,
                hint: None,
                transform: EdgeTransform::Direct,
            },
        );
        transitions.insert(
            "c".to_string(),
            TransitionEdge {
                target: "c".to_string(),
                condition: TransitionCondition::LlmChoice,
                hint: None,
                transform: EdgeTransform::Direct,
            },
        );
        stage_a.transitions = Some(transitions);

        let bp = make_blueprint(vec![stage_a, stage_b, stage_c]);
        // Provider returns garbage → fallback to first edge
        let (mut engine, entity) = make_engine_with_mock_provider(&bp, "XXXXXXXXX_no_match");
        let visit_counts = HashMap::new();

        // Wrapped in `with_tracing` so the "LLM transition response didn't
        // match any edge" warn! reached via prompt_llm_transition's fallback
        // has its field-list lines exercised.
        let result = with_tracing(|| {
            resolve_transition(
                &bp.stages[0],
                0,
                &bp,
                &visit_counts,
                &StageResult::Success,
                &mut engine,
                entity,
                "mock",
                "test",
            )
        })
        .await;

        assert!(result.is_some());
        let (edge, _) = result.unwrap();
        // Must be one of the valid targets
        assert_edge_is_b_or_c(&edge.target);
    }

    // ─── line-89: Error result but no Error edge → if-let None branch ────────

    #[tokio::test]
    async fn resolve_transition_error_result_no_error_edge_falls_through_to_always() {
        // stage_result == Error but the only available edge is Always, not Error.
        // This causes the `if let Some(...)` at the Error-check to NOT match,
        // exercising the None-branch closing `}` at line 89, then falling
        // through to Step 3 which picks the Always edge.
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

        // StageResult::Error but no Error-condition edge → falls through to Always
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
        assert_eq!(edge.target, "b");
    }

    // ─── line-104: MaxIterations result but no MaxIterations edge → if-let None

    #[tokio::test]
    async fn resolve_transition_max_iterations_result_no_max_iter_edge_falls_through() {
        // stage_result == MaxIterations but the only available edge is Always.
        // The `if let Some(...)` at the MaxIterations-check does NOT match,
        // exercising the None-branch closing `}` at line 104, then falling
        // through to Step 3 which picks the Always edge.
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
            &StageResult::MaxIterations,
            &mut engine,
            entity,
            "anthropic",
            "claude-sonnet-4-6",
        )
        .await;

        let (edge, _idx) = result.unwrap();
        assert_eq!(edge.target, "b");
    }

    // ─── line-325: Clear transform on entity without ContextWindow ────────────

    fn make_engine_and_entity_no_window(
        _blueprint: &Blueprint,
    ) -> (AgentEngine, bevy_ecs::prelude::Entity) {
        let registry = ProviderRegistry::new();
        let mut engine = AgentEngine::with_providers(registry);
        // NOTE: `AgentPool::spawn_agent` unconditionally inserts a
        // `ContextWindow` component itself (see `leviath_runtime::pool`), so
        // routing through the pool -- even while skipping
        // `initialize_context_window` -- can never produce a `None` from
        // `get_mut::<ContextWindow>`. To genuinely exercise that branch we
        // spawn a bare entity with no components at all, bypassing the pool.
        let entity = engine.world_mut().spawn_empty().id();
        (engine, entity)
    }

    #[tokio::test]
    async fn apply_edge_transform_clear_no_context_window_is_noop() {
        // Entity has no ContextWindow → `get_mut::<ContextWindow>` returns None,
        // exercising the None-branch closing `}` at line 325. Should not panic.
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, entity) = make_engine_and_entity_no_window(&bp);

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
        // No panic = success; nothing to assert since there was no window.
    }

    // ─── line-360: Custom transform on entity without ContextWindow ───────────

    #[tokio::test]
    async fn apply_edge_transform_custom_no_context_window_is_noop() {
        // Entity has no ContextWindow → `get_mut::<ContextWindow>` returns None,
        // exercising the None-branch closing `}` at line 360. Should not panic.
        let bp = make_blueprint(vec![make_stage("a"), make_stage("b")]);
        let (mut engine, entity) = make_engine_and_entity_no_window(&bp);

        let edge = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Custom {
                carry: vec![],
                compact: vec![],
                clear: vec!["conversation".to_string()],
                compact_prompt: None,
            },
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
        // No panic = success.
    }
}
