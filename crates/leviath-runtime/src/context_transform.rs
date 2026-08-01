//! Inter-agent context transforms: seed a freshly-spawned child agent's context
//! from its parent's, when the blueprints declare a mapping.
//!
//! A [`Blueprint`](leviath_core::Blueprint) may declare
//! [`ContextTransform`](leviath_core::blueprint::ContextTransform)s - `{from_blueprint,
//! to_blueprint, mappings}` - describing how a parent's context regions flow into
//! a child's when the parent (blueprint A) spawns a child (blueprint B). This is
//! how an agent hands work down the tree: the planner's plan region becomes the
//! implementer's task region, findings become inputs, etc.
//!
//! [`apply_context_transforms`] is invoked right after a child is spawned and
//! linked (sub-agent spawn and fan-out worker start). It looks up a transform
//! matching `(parent_blueprint → child_blueprint)` in either blueprint's
//! `transforms`, and for each [`RegionMapping`] copies the parent's `from_region`
//! into the child's `to_region`, applying the optional [`ContentTransform`].

use bevy_ecs::prelude::*;
use leviath_core::blueprint::{ContentTransform, RegionMapping};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::compaction_bridge::{CompactionJob, CompactionOutcome, run_compaction_job};
use crate::components::{AgentState, AgentStatus, ContextWindow};
use crate::pipeline::{AgentBlueprint, CompactionSettings, InferenceStage, Providers};

/// Seed `child`'s context from `parent`'s per a declared blueprint transform.
/// No-op unless both carry an [`AgentBlueprint`] with different names and a
/// matching [`ContextTransform`](leviath_core::blueprint::ContextTransform) exists.
pub fn apply_context_transforms(world: &mut World, parent: Entity, child: Entity) {
    let Some(mappings) = collect_transform_mappings(world, parent, child) else {
        return;
    };
    // Read the parent's mapped regions into owned, already-transformed content
    // (immutable borrow of the parent), then write them into the child.
    let mut writes: Vec<(String, String)> = Vec::new();
    // Summarize mappings write their raw content now (fallback) and are queued
    // for deferred LLM summarization, which replaces the region on a later tick.
    let mut to_summarize: Vec<(String, String)> = Vec::new();
    if let Some(parent_window) = world.get::<ContextWindow>(parent) {
        for m in &mappings {
            if let Some(region) = parent_window.get_region(&m.from_region) {
                let joined = region
                    .content
                    .iter()
                    .map(|e| e.content())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !joined.is_empty() {
                    let content = apply_content_transform(&joined, &m.transform);
                    if matches!(m.transform, Some(ContentTransform::Summarize)) {
                        to_summarize.push((m.to_region.clone(), content.clone()));
                    }
                    writes.push((m.to_region.clone(), content));
                }
            }
        }
    }
    let mut wrote_to_child = false;
    if let Some(mut child_window) = world.get_mut::<ContextWindow>(child) {
        for (to_region, content) in writes {
            let tokens = leviath_core::estimate_tokens(&content);
            let _ = child_window.add_to_region(&to_region, content, tokens);
        }
        wrote_to_child = true;
    }
    // Queue any Summarize regions for the deferred summary lane (raw content is
    // already in place as a fallback if summarization can't run).
    if wrote_to_child && !to_summarize.is_empty() {
        world
            .entity_mut(child)
            .insert(PendingContentSummary(to_summarize));
    }
}

// ─── Summarize transform lane (deferred LLM summarization) ───────────────────

/// A freshly-spawned child's `Summarize`-transform regions, queued for LLM
/// summarization as `(child_region, raw_content)` pairs. The raw content is
/// already in each region (a fallback); the summary replaces it once ready.
#[derive(Component, Debug, Clone)]
pub struct PendingContentSummary(pub Vec<(String, String)>);

/// A child's content-summary job is in flight on the summary lane.
#[derive(Component, Debug, Clone, Copy)]
pub struct AwaitingContentSummary;

/// The receiving side of the content-summary lane, drained by
/// [`collect_content_summary`]. (The sending side is
/// `InferenceStage::content_summary_outcomes`.)
#[derive(Resource)]
pub struct ContentSummaryResults(pub UnboundedReceiver<CompactionOutcome>);

/// Dispatch: for each child with queued [`PendingContentSummary`] regions, build
/// one summarize request per region and run it on the summary lane. Mirrors
/// [`dispatch_compaction`](crate::pipeline::dispatch_compaction), reusing the
/// same worker + per-model pool. A child with no [`CompactionSettings`] or no
/// registered provider can't summarize, so it keeps its raw content; a full pool
/// just retries next tick.
pub fn dispatch_content_summary(
    agents: Query<(
        Entity,
        &AgentState,
        &PendingContentSummary,
        Option<&CompactionSettings>,
    )>,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, state, pending, settings) in agents.iter() {
        crate::tick_scope::enter(entity);
        if state.status != AgentStatus::Active {
            continue; // paused / cancelled - don't start new work
        }
        let Some(settings) = settings else {
            // No compaction config ⇒ can't summarize; keep the raw content.
            commands.entity(entity).remove::<PendingContentSummary>();
            continue;
        };
        let config = &settings.0;
        let Some(provider) = providers.0.get(&config.provider) else {
            // Provider not registered ⇒ keep the raw content.
            commands.entity(entity).remove::<PendingContentSummary>();
            continue;
        };
        let Some(permit) = stage.pools.try_acquire(&config.model) else {
            continue; // pool full - retry next tick (keep the pending marker)
        };
        let requests = pending
            .0
            .iter()
            .map(|(region, content)| {
                (
                    region.clone(),
                    crate::pipeline::compaction_request(config, content, region),
                )
            })
            .collect();
        stage.runtime.spawn(run_compaction_job(
            CompactionJob {
                entity,
                provider,
                requests,
                permit,
            },
            std::time::Duration::from_secs(leviath_providers::DEFAULT_INFERENCE_TIMEOUT_SECS),
            stage.content_summary_outcomes.clone(),
            stage.wake.clone(),
        ));
        commands
            .entity(entity)
            .remove::<PendingContentSummary>()
            .insert(AwaitingContentSummary);
    }
}

/// Collect: apply each completed content summary, replacing the child region's
/// raw content with the summary. A provider error leaves the raw content in
/// place (best-effort, like compaction).
pub fn collect_content_summary(
    mut results: ResMut<ContentSummaryResults>,
    mut agents: Query<&mut ContextWindow, With<AwaitingContentSummary>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    while let Ok(outcome) = results.0.try_recv() {
        let Ok(mut window) = agents.get_mut(outcome.entity) else {
            continue; // stale: child cancelled/despawned since dispatch
        };
        crate::tick_scope::enter(outcome.entity);
        if let Ok(summaries) = outcome.result {
            for (region, summary) in summaries {
                let tokens = leviath_core::estimate_tokens(&summary);
                window.replace_region(&region, summary, tokens);
            }
        }
        commands
            .entity(outcome.entity)
            .remove::<AwaitingContentSummary>();
    }
}

/// Find the region mappings for `parent_blueprint → child_blueprint`, searching
/// the parent's then the child's `transforms`. `None` when either lacks a
/// blueprint, they share a name (no cross-blueprint mapping), or no non-empty
/// transform matches.
fn collect_transform_mappings(
    world: &World,
    parent: Entity,
    child: Entity,
) -> Option<Vec<RegionMapping>> {
    let parent_name = world.get::<AgentBlueprint>(parent)?.0.name.clone();
    let child_name = world.get::<AgentBlueprint>(child)?.0.name.clone();
    if parent_name == child_name {
        return None;
    }
    for entity in [parent, child] {
        // Both blueprints are guaranteed present by the `?`s above.
        let bp = world
            .get::<AgentBlueprint>(entity)
            .expect("parent/child blueprint checked above");
        let found =
            bp.0.transforms
                .iter()
                .find(|t| t.from_blueprint == parent_name && t.to_blueprint == child_name)
                .map(|t| t.mappings.clone())
                .filter(|m| !m.is_empty());
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Apply a region mapping's optional content transform.
fn apply_content_transform(content: &str, transform: &Option<ContentTransform>) -> String {
    match transform {
        None | Some(ContentTransform::Direct) => content.to_string(),
        Some(ContentTransform::Extract { fields }) => extract_fields(content, fields),
        // Summarize needs an async LLM call that isn't available at spawn time;
        // fall back to a direct copy so the data still transfers. (Follow-up:
        // route through the compaction lane - tracked separately.)
        Some(ContentTransform::Summarize) => content.to_string(),
    }
}

/// Keep only the named fields of a JSON-object `content` (pretty-printed); return
/// `content` unchanged when it isn't a JSON object.
fn extract_fields(content: &str, fields: &[String]) -> String {
    match serde_json::from_str::<serde_json::Value>(content) {
        Ok(serde_json::Value::Object(map)) => {
            let filtered: serde_json::Map<String, serde_json::Value> = fields
                .iter()
                .filter_map(|f| map.get(f).map(|v| (f.clone(), v.clone())))
                .collect();
            serde_json::to_string_pretty(&serde_json::Value::Object(filtered))
                .expect("a JSON object always serializes")
        }
        _ => content.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::blueprint::{ContextTransform, RegionMapping};
    use leviath_core::{Region, RegionKind};

    fn bp_with_transforms(name: &str, transforms: Vec<ContextTransform>) -> AgentBlueprint {
        let layout = leviath_core::layout::ContextLayout::new(vec![], 10_000);
        let mut bp = leviath_core::Blueprint::new(
            name.to_string(),
            "d".to_string(),
            vec![leviath_core::Stage::new(
                "s".to_string(),
                leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
            )],
            layout,
        );
        bp.transforms = transforms;
        AgentBlueprint(bp)
    }

    fn window_with(regions: &[(&str, &str)]) -> ContextWindow {
        let mut w = ContextWindow::new(100_000);
        for (name, content) in regions {
            w.add_region(Region::new(name.to_string(), RegionKind::Clearable, 10_000));
            if !content.is_empty() {
                w.add_to_region(name, content.to_string(), 5).unwrap();
            }
        }
        w
    }

    fn mapping(from: &str, to: &str, transform: Option<ContentTransform>) -> RegionMapping {
        RegionMapping {
            from_region: from.to_string(),
            to_region: to.to_string(),
            transform,
        }
    }

    fn transform(from_bp: &str, to_bp: &str, mappings: Vec<RegionMapping>) -> ContextTransform {
        ContextTransform {
            from_blueprint: from_bp.to_string(),
            to_blueprint: to_bp.to_string(),
            mappings,
        }
    }

    // ── pure helpers ──

    #[test]
    fn apply_content_transform_variants() {
        assert_eq!(apply_content_transform("x", &None), "x");
        assert_eq!(
            apply_content_transform("x", &Some(ContentTransform::Direct)),
            "x"
        );
        assert_eq!(
            apply_content_transform("x", &Some(ContentTransform::Summarize)),
            "x"
        );
        let out = apply_content_transform(
            r#"{"a":1,"b":2}"#,
            &Some(ContentTransform::Extract {
                fields: vec!["a".to_string()],
            }),
        );
        assert!(out.contains("\"a\""));
        assert!(!out.contains("\"b\""));
    }

    #[test]
    fn extract_fields_handles_objects_missing_fields_and_non_objects() {
        // Object: keep only present named fields.
        let out = extract_fields(r#"{"a":1,"b":2}"#, &["a".to_string(), "z".to_string()]);
        assert!(out.contains("\"a\""));
        assert!(!out.contains("\"b\""));
        assert!(!out.contains("\"z\"")); // absent field skipped
        // Non-object JSON ⇒ unchanged.
        assert_eq!(extract_fields("[1,2]", &["a".to_string()]), "[1,2]");
        // Non-JSON ⇒ unchanged.
        assert_eq!(
            extract_fields("plain text", &["a".to_string()]),
            "plain text"
        );
    }

    // ── mapping resolution ──

    #[test]
    fn collect_mappings_finds_on_parent_then_child_and_rejects_mismatches() {
        let m = vec![mapping("plan", "task", None)];
        // Declared on the parent.
        let mut w = World::new();
        let p = w
            .spawn(bp_with_transforms(
                "planner",
                vec![transform("planner", "coder", m.clone())],
            ))
            .id();
        let c = w.spawn(bp_with_transforms("coder", vec![])).id();
        assert_eq!(collect_transform_mappings(&w, p, c).unwrap().len(), 1);

        // Declared on the child instead.
        let mut w2 = World::new();
        let p2 = w2.spawn(bp_with_transforms("planner", vec![])).id();
        let c2 = w2
            .spawn(bp_with_transforms(
                "coder",
                vec![transform("planner", "coder", m.clone())],
            ))
            .id();
        assert_eq!(collect_transform_mappings(&w2, p2, c2).unwrap().len(), 1);

        // Same blueprint name ⇒ no mapping.
        let mut w3 = World::new();
        let p3 = w3
            .spawn(bp_with_transforms(
                "same",
                vec![transform("same", "same", m.clone())],
            ))
            .id();
        let c3 = w3.spawn(bp_with_transforms("same", vec![])).id();
        assert!(collect_transform_mappings(&w3, p3, c3).is_none());

        // No matching transform (wrong target) ⇒ none.
        let mut w4 = World::new();
        let p4 = w4
            .spawn(bp_with_transforms(
                "planner",
                vec![transform("planner", "other", m.clone())],
            ))
            .id();
        let c4 = w4.spawn(bp_with_transforms("coder", vec![])).id();
        assert!(collect_transform_mappings(&w4, p4, c4).is_none());

        // Child missing a blueprint ⇒ none (the child `?`).
        let mut w5 = World::new();
        let p5 = w5.spawn(bp_with_transforms("planner", vec![])).id();
        let c5 = w5.spawn_empty().id();
        assert!(collect_transform_mappings(&w5, p5, c5).is_none());

        // Parent missing a blueprint ⇒ none (the parent `?`).
        let mut w6 = World::new();
        let p6 = w6.spawn_empty().id();
        let c6 = w6.spawn(bp_with_transforms("coder", vec![])).id();
        assert!(collect_transform_mappings(&w6, p6, c6).is_none());
    }

    // ── end-to-end application ──

    #[test]
    fn apply_context_transforms_copies_and_transforms_regions() {
        let mut w = World::new();
        let parent = w
            .spawn((
                bp_with_transforms(
                    "planner",
                    vec![transform(
                        "planner",
                        "coder",
                        vec![
                            mapping("plan", "task", Some(ContentTransform::Direct)),
                            mapping("empty", "unused", None), // empty ⇒ skipped
                            mapping("absent", "ghost", None), // region not in parent ⇒ skipped
                            mapping(
                                "data",
                                "inputs",
                                Some(ContentTransform::Extract {
                                    fields: vec!["keep".to_string()],
                                }),
                            ),
                        ],
                    )],
                ),
                window_with(&[
                    ("plan", "the plan"),
                    ("empty", ""),
                    ("data", r#"{"keep":1,"drop":2}"#),
                ]),
            ))
            .id();
        let child = w
            .spawn((
                bp_with_transforms("coder", vec![]),
                window_with(&[("task", ""), ("inputs", "")]),
            ))
            .id();

        apply_context_transforms(&mut w, parent, child);

        let cw = w.get::<ContextWindow>(child).unwrap();
        let task = cw.get_region("task").unwrap();
        assert!(task.current_tokens > 0);
        assert_eq!(task.content[0].content(), "the plan");
        let inputs = cw.get_region("inputs").unwrap();
        assert!(inputs.content[0].content().contains("\"keep\""));
        assert!(!inputs.content[0].content().contains("\"drop\""));
    }

    #[test]
    fn apply_context_transforms_noop_without_a_matching_transform_or_windows() {
        // No transform ⇒ nothing copied.
        let mut w = World::new();
        let p = w
            .spawn((
                bp_with_transforms("a", vec![]),
                window_with(&[("plan", "x")]),
            ))
            .id();
        let c = w
            .spawn((
                bp_with_transforms("b", vec![]),
                window_with(&[("task", "")]),
            ))
            .id();
        apply_context_transforms(&mut w, p, c);
        assert_eq!(
            w.get::<ContextWindow>(c)
                .unwrap()
                .get_region("task")
                .unwrap()
                .current_tokens,
            0
        );

        // Matching transform but the parent has no window ⇒ no panic, nothing copied.
        let m = vec![mapping("plan", "task", None)];
        let mut w2 = World::new();
        let p2 = w2
            .spawn(bp_with_transforms("a", vec![transform("a", "b", m)]))
            .id(); // no window
        let c2 = w2
            .spawn((
                bp_with_transforms("b", vec![]),
                window_with(&[("task", "")]),
            ))
            .id();
        apply_context_transforms(&mut w2, p2, c2);
        assert_eq!(
            w2.get::<ContextWindow>(c2)
                .unwrap()
                .get_region("task")
                .unwrap()
                .current_tokens,
            0
        );

        // Matching transform + parent content, but the child has no window ⇒
        // no panic, nothing to write.
        let m2 = vec![mapping("plan", "task", None)];
        let mut w3 = World::new();
        let p3 = w3
            .spawn((
                bp_with_transforms("a", vec![transform("a", "b", m2)]),
                window_with(&[("plan", "content")]),
            ))
            .id();
        let c3 = w3.spawn(bp_with_transforms("b", vec![])).id(); // no window
        apply_context_transforms(&mut w3, p3, c3);
        assert!(w3.get::<ContextWindow>(c3).is_none());
    }

    // ── Summarize transform lane ──

    use crate::components::AgentStatus;
    use crate::inference_pool::{InferencePoolConfig, InferencePools};
    use crate::providers::ProviderRegistry;
    use leviath_providers::{
        FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, Provider,
        ProviderError, TokenUsage,
    };
    use std::sync::Arc;
    use tokio::runtime::Handle;
    use tokio::sync::Notify;
    use tokio::sync::mpsc;

    struct FakeProvider {
        reply: String,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl Provider for FakeProvider {
        async fn infer(
            &self,
            _req: InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            if self.fail {
                return Err(ProviderError::Other("boom".to_string()));
            }
            Ok(InferenceResponse {
                content: self.reply.clone(),
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
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "fake"
        }
        fn capabilities(&self, _m: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }

    fn agent_state(status: AgentStatus) -> AgentState {
        AgentState {
            agent_id: "child".to_string(),
            current_stage: "s".to_string(),
            iteration: 0,
            status,
            spawned_children_ids: vec![],
            pending_wait: None,
            accepts_messages: true,
        }
    }

    fn settings() -> CompactionSettings {
        CompactionSettings(leviath_core::CompactionConfig {
            provider: "p".to_string(),
            model: "m".to_string(),
            system_prompt: None,
            user_prompt_template: None,
            max_summary_tokens: 200,
            temperature: 0.2,
        })
    }

    /// A world with the summary lane wired: a `Providers` registry (with the
    /// fake provider registered iff `register`), an `InferenceStage` over
    /// `pools`, and the returned receiver for the content-summary outcomes.
    fn summary_world(
        register: bool,
        fail: bool,
        pools: InferencePools,
    ) -> (World, mpsc::UnboundedReceiver<CompactionOutcome>) {
        let mut registry = ProviderRegistry::new();
        if register {
            registry.register(
                "p".to_string(),
                Arc::new(FakeProvider {
                    reply: "SUMMARY".to_string(),
                    fail,
                }),
            );
        }
        let (cs_tx, cs_rx) = mpsc::unbounded_channel();
        let (a, _a) = mpsc::unbounded_channel();
        let (b, _b) = mpsc::unbounded_channel();
        let (c, _c) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(Providers(registry));
        world.insert_resource(InferenceStage {
            pools: Arc::new(pools),
            outcomes: a,
            transition_outcomes: b,
            compaction_outcomes: c,
            content_summary_outcomes: cs_tx,
            wake: Arc::new(Notify::new()),
            runtime: Handle::current(),
            exact_token_counting: false,
        });
        (world, cs_rx)
    }

    fn run_dispatch(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(dispatch_content_summary);
        s.run(world);
    }

    #[tokio::test]
    async fn fake_provider_metadata_is_exercised() {
        let p = FakeProvider {
            reply: String::new(),
            fail: false,
        };
        assert_eq!(p.name(), "fake");
        assert_eq!(p.count_tokens("t", "m").await, 1);
        assert_eq!(p.max_context_tokens("m"), 100_000);
        let _ = p.capabilities("m");
    }

    #[test]
    fn apply_context_transforms_queues_a_summarize_mapping() {
        let mut w = World::new();
        let parent = w
            .spawn((
                bp_with_transforms(
                    "planner",
                    vec![transform(
                        "planner",
                        "coder",
                        vec![mapping("plan", "task", Some(ContentTransform::Summarize))],
                    )],
                ),
                window_with(&[("plan", "the long plan")]),
            ))
            .id();
        let child = w
            .spawn((
                bp_with_transforms("coder", vec![]),
                window_with(&[("task", "")]),
            ))
            .id();

        apply_context_transforms(&mut w, parent, child);

        // Raw content is written now (fallback)...
        assert_eq!(
            w.get::<ContextWindow>(child)
                .unwrap()
                .get_region("task")
                .unwrap()
                .content[0]
                .content(),
            "the long plan"
        );
        // ...and the region is queued for deferred summarization.
        let pending = w.get::<PendingContentSummary>(child).unwrap();
        assert_eq!(
            pending.0,
            vec![("task".to_string(), "the long plan".to_string())]
        );
    }

    #[tokio::test]
    async fn dispatch_summarizes_and_marks_awaiting() {
        let (mut world, mut rx) =
            summary_world(true, false, InferencePools::new(InferencePoolConfig::new()));
        let e = world
            .spawn((
                agent_state(AgentStatus::Active),
                settings(),
                PendingContentSummary(vec![("task".to_string(), "raw".to_string())]),
            ))
            .id();
        run_dispatch(&mut world);
        assert!(world.get::<PendingContentSummary>(e).is_none());
        assert!(world.get::<AwaitingContentSummary>(e).is_some());
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let outcome = rx.try_recv().expect("summary job ran");
        assert_eq!(outcome.entity, e);
        assert_eq!(
            outcome.result.unwrap(),
            vec![("task".to_string(), "SUMMARY".to_string())]
        );
    }

    #[tokio::test]
    async fn dispatch_without_settings_or_provider_drops_pending() {
        // No CompactionSettings ⇒ can't summarize ⇒ drop pending, keep raw.
        let (mut world, _rx) =
            summary_world(true, false, InferencePools::new(InferencePoolConfig::new()));
        let e = world
            .spawn((
                agent_state(AgentStatus::Active),
                PendingContentSummary(vec![("task".to_string(), "raw".to_string())]),
            ))
            .id();
        run_dispatch(&mut world);
        assert!(world.get::<PendingContentSummary>(e).is_none());
        assert!(world.get::<AwaitingContentSummary>(e).is_none());

        // Provider not registered ⇒ drop pending too.
        let (mut world2, _rx2) = summary_world(
            false,
            false,
            InferencePools::new(InferencePoolConfig::new()),
        );
        let e2 = world2
            .spawn((
                agent_state(AgentStatus::Active),
                settings(),
                PendingContentSummary(vec![("task".to_string(), "raw".to_string())]),
            ))
            .id();
        run_dispatch(&mut world2);
        assert!(world2.get::<PendingContentSummary>(e2).is_none());
        assert!(world2.get::<AwaitingContentSummary>(e2).is_none());
    }

    #[tokio::test]
    async fn dispatch_keeps_pending_on_full_pool_and_skips_non_active() {
        // Full pool (limit 0) ⇒ retry next tick (pending stays, no awaiting).
        let (mut world, _rx) = summary_world(
            true,
            false,
            InferencePools::new(InferencePoolConfig::new().with_default(Some(0))),
        );
        let e = world
            .spawn((
                agent_state(AgentStatus::Active),
                settings(),
                PendingContentSummary(vec![("task".to_string(), "raw".to_string())]),
            ))
            .id();
        run_dispatch(&mut world);
        assert!(world.get::<PendingContentSummary>(e).is_some());
        assert!(world.get::<AwaitingContentSummary>(e).is_none());

        // A non-active child is skipped entirely.
        let (mut world2, _rx2) =
            summary_world(true, false, InferencePools::new(InferencePoolConfig::new()));
        let e2 = world2
            .spawn((
                agent_state(AgentStatus::Waiting),
                settings(),
                PendingContentSummary(vec![("task".to_string(), "raw".to_string())]),
            ))
            .id();
        run_dispatch(&mut world2);
        assert!(world2.get::<PendingContentSummary>(e2).is_some());
    }

    fn run_collect(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(collect_content_summary);
        s.run(world);
    }

    #[test]
    fn collect_replaces_region_with_summary_and_leaves_raw_on_error() {
        let mut world = World::new();
        let (tx, rx) = mpsc::unbounded_channel();
        world.insert_resource(ContentSummaryResults(rx));
        let e = world
            .spawn((
                AwaitingContentSummary,
                window_with(&[("task", "raw content")]),
            ))
            .id();
        // A successful summary replaces the region's content.
        tx.send(CompactionOutcome {
            entity: e,
            result: Ok(vec![("task".to_string(), "SHORT".to_string())]),
        })
        .unwrap();
        run_collect(&mut world);
        assert_eq!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("task")
                .unwrap()
                .content[0]
                .content(),
            "SHORT"
        );
        assert!(world.get::<AwaitingContentSummary>(e).is_none());

        // An error leaves the raw content in place.
        let e2 = world
            .spawn((AwaitingContentSummary, window_with(&[("task", "keep me")])))
            .id();
        tx.send(CompactionOutcome {
            entity: e2,
            result: Err(leviath_providers::ProviderError::Other("x".to_string())),
        })
        .unwrap();
        // A stale (despawned) entity is skipped without panic.
        tx.send(CompactionOutcome {
            entity: Entity::from_raw_u32(9999)
                .expect("a small literal index is always a valid entity id"),
            result: Ok(vec![("task".to_string(), "ignored".to_string())]),
        })
        .unwrap();
        run_collect(&mut world);
        assert_eq!(
            world
                .get::<ContextWindow>(e2)
                .unwrap()
                .get_region("task")
                .unwrap()
                .content[0]
                .content(),
            "keep me"
        );
        assert!(world.get::<AwaitingContentSummary>(e2).is_none());
    }
}
