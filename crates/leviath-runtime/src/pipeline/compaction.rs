//! Threshold compaction and edge-transform compaction, over the compaction lane.

use super::*;

// ─── Compaction (LLM context summarization) ──────────────────────────────────

/// Per-agent compaction configuration; its presence opts the agent into
/// automatic eviction + LLM compaction before each inference (mirrors the
/// imperative loop's `Option<&CompactionConfig>`).
#[derive(Component, Clone)]
pub struct CompactionSettings(pub leviath_core::CompactionConfig);

/// A compaction job (LLM summarization) is in flight; the agent is held out of
/// inference until its summaries land.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AwaitingCompaction;

/// The receiving end of the compaction-outcomes channel, as a world resource.
/// (The sending end lives in [`InferenceStage::compaction_outcomes`].)
#[derive(Resource)]
pub struct CompactionResults(pub UnboundedReceiver<CompactionOutcome>);

/// The eviction threshold (fraction of budget) at which compaction kicks in -
/// the same 0.9 the imperative `evict_and_compact` uses.
pub(crate) const EVICTION_THRESHOLD: f32 = 0.9;

/// Spawn a compaction job under the lane supervisor, so a job that dies without
/// reporting still produces an outcome.
///
/// Compaction is best-effort, but *waiting* for it is not: the agent is held
/// `AwaitingCompaction` until an outcome lands. A lost job would park it there
/// for good. The synthesized error takes the collect system's failure path,
/// which returns the agent to `ReadyToInfer` with its context untouched - the
/// same place a genuine summarization failure leaves it.
fn spawn_supervised_compaction(stage: &InferenceStage, entity: Entity, job: CompactionJob) {
    let lost_outcomes = stage.compaction_outcomes.clone();
    let lost_wake = stage.wake.clone();
    crate::lane_supervisor::spawn_supervised(
        &stage.runtime,
        "compaction",
        run_compaction_job(
            job,
            std::time::Duration::from_secs(leviath_providers::DEFAULT_INFERENCE_TIMEOUT_SECS),
            stage.compaction_outcomes.clone(),
            stage.wake.clone(),
        ),
        move |message| {
            let _ = lost_outcomes.send(CompactionOutcome {
                entity,
                result: Err(leviath_providers::ProviderError::Other(message)),
            });
            lost_wake.notify_one();
        },
    );
}

/// What `dispatch_compaction` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type CompactionQuery = (
    Entity,
    &'static AgentState,
    &'static mut ContextWindow,
    &'static CompactionSettings,
);

/// Compaction-dispatch system: for each `ReadyToInfer` agent with
/// [`CompactionSettings`] whose window is over the eviction threshold, do the
/// synchronous eviction inline; if that surfaces regions needing LLM
/// summarization (and content to summarize), build one request per region,
/// acquire a permit for the compaction model, spawn the job, and hold the agent
/// as `AwaitingCompaction`. Anything that can't proceed (under threshold, nothing
/// to summarize, provider missing, pool full) simply leaves the agent
/// `ReadyToInfer` so inference proceeds - compaction is best-effort. (Ported from
/// `AgentEngine::evict_and_compact`.)
pub fn dispatch_compaction(
    mut agents: Query<CompactionQuery, (With<ReadyToInfer>, Without<AwaitingCompaction>)>,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, state, mut window, settings) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if state.status != AgentStatus::Active {
            continue; // paused / waiting / cancelled - don't start new work
        }
        if !window.needs_eviction(EVICTION_THRESHOLD) {
            continue; // under threshold - nothing to do
        }
        let target_free = window.max_tokens / 10;
        let Ok(eviction) = window.try_evict(target_free) else {
            continue; // couldn't evict - proceed to inference as-is
        };

        // Build a summarize request per region that both needs compaction and
        // has content to summarize.
        let config = &settings.0;
        let mut requests = Vec::new();
        for region_name in &eviction.needs_compaction {
            // The names come from `try_evict`'s own scan of `window.regions`, and
            // nothing between there and here mutates the region set, so the region
            // is guaranteed present.
            let region = window
                .get_region(region_name)
                .expect("needs_compaction region present: named by try_evict's own scan");
            let content: String = region
                .content
                .iter()
                .map(|e| e.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            if content.is_empty() {
                continue; // nothing to summarize (e.g. token-only placeholder)
            }
            requests.push((
                region_name.clone(),
                compaction_request(config, &content, region_name),
            ));
        }
        if requests.is_empty() {
            continue; // sync eviction was enough (or nothing summarizable)
        }

        let Some(provider) = providers.0.get(&config.provider) else {
            continue; // compaction provider not registered - skip, non-fatal
        };
        let Some(permit) = stage.pools.try_acquire(&config.model) else {
            continue; // pool full - skip compaction this round
        };

        spawn_supervised_compaction(
            &stage,
            entity,
            CompactionJob {
                entity,
                provider,
                requests,
                permit,
            },
        );
        commands
            .entity(entity)
            .remove::<ReadyToInfer>()
            .insert(AwaitingCompaction);
    }
}

/// Compaction-collect system: drain finished compaction jobs and apply each
/// summary into its paired `CompactHistory` region, clearing the summarized
/// source region. A provider error leaves the context untouched (best-effort).
/// Either way the agent returns to `ReadyToInfer`. (Ported from the storage tail
/// of `AgentEngine::compact_region`.)
pub fn collect_compaction(
    mut results: ResMut<CompactionResults>,
    mut agents: Query<
        (
            &mut ContextWindow,
            Option<&mut crate::telemetry::StageActivity>,
        ),
        With<AwaitingCompaction>,
    >,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    while let Ok(outcome) = results.0.try_recv() {
        let Ok((mut window, activity)) = agents.get_mut(outcome.entity) else {
            continue; // stale: agent cancelled/despawned since dispatch
        };
        crate::tick_scope::enter(outcome.entity);
        if let Some(mut activity) = activity {
            activity
                .0
                .push(crate::telemetry::ActivityRecord::Compaction {
                    success: outcome.result.is_ok(),
                });
        }
        if let Ok(summaries) = outcome.result {
            for (region_name, summary) in summaries {
                let summary_tokens = leviath_core::estimate_tokens(&summary);
                let history = window
                    .regions
                    .iter()
                    .find(|r| {
                        matches!(&r.kind, leviath_core::RegionKind::CompactHistory { source_region }
                            if source_region == &region_name)
                    })
                    .map(|r| r.name.clone());
                if let Some(history_name) = history {
                    let _ = window.add_to_region(&history_name, summary, summary_tokens);
                }
                if let Some(region) = window.get_region_mut(&region_name) {
                    region.clear();
                }
            }
            window.current_tokens = window.calculate_tokens();
        }
        commands
            .entity(outcome.entity)
            .remove::<AwaitingCompaction>()
            .insert(ReadyToInfer);
    }
}

/// Build the summarize [`InferenceRequest`] for one region's content.
pub(crate) fn compaction_request(
    config: &leviath_core::CompactionConfig,
    content: &str,
    region_name: &str,
) -> InferenceRequest {
    InferenceRequest {
        system: vec![],
        messages: vec![
            leviath_providers::Message {
                role: "system".to_string(),
                content: config.system_prompt().to_string().into(),
                cache_breakpoint: false,
            },
            leviath_providers::Message {
                role: "user".to_string(),
                content: config.user_prompt(content, region_name).into(),
                cache_breakpoint: false,
            },
        ],
        model: config.model.clone(),
        max_tokens: config.max_summary_tokens,
        temperature: config.temperature,
        tools: Vec::new(),
        extra: serde_json::Value::Null,
        request_timeout_secs: None,
    }
}

// ─── Edge transforms (context reshaping on stage transitions) ────────────────

/// Regions an edge transform asked to LLM-compact after a transition, awaiting
/// the compaction lane (drained by [`dispatch_edge_compact`]).
#[derive(Component, Debug, Clone)]
pub struct PendingEdgeCompact(pub Vec<String>);

/// Whether a region kind is "stage-specific" - eligible for an edge transform to
/// clear or compact. The always-preserved kinds (pinned identity, compaction
/// history, hashmap stores, persistent custom regions) are never touched.
pub(crate) fn is_stage_specific(kind: &leviath_core::RegionKind) -> bool {
    !matches!(
        kind,
        leviath_core::RegionKind::Pinned
            | leviath_core::RegionKind::CompactHistory { .. }
            | leviath_core::RegionKind::HashMap { .. }
            | leviath_core::RegionKind::Custom {
                persistent: true,
                ..
            }
    )
}

/// Apply an edge transform's **synchronous** effects to the outgoing window
/// (clearing stage-specific / named regions) and return the names of regions the
/// caller should hand to the LLM compaction lane. (Ported from the deleted
/// `graph::apply_edge_transform`; `Direct` on a linear/chosen edge carries context
/// as-is.)
pub(crate) fn apply_edge_transform(
    window: &mut ContextWindow,
    transform: &leviath_core::blueprint::EdgeTransform,
) -> Vec<String> {
    use leviath_core::blueprint::EdgeTransform;
    match transform {
        EdgeTransform::Direct => Vec::new(),
        EdgeTransform::Clear => {
            window
                .regions
                .iter_mut()
                .filter(|r| is_stage_specific(&r.kind))
                .for_each(|r| r.clear());
            window.current_tokens = window.calculate_tokens();
            Vec::new()
        }
        EdgeTransform::Compact { .. } => window
            .regions
            .iter()
            .filter(|r| is_stage_specific(&r.kind) && !r.content.is_empty())
            .map(|r| r.name.clone())
            .collect(),
        EdgeTransform::Custom {
            carry,
            compact,
            clear,
            ..
        } => {
            clear
                .iter()
                .filter(|n| !carry.contains(n))
                .for_each(|name| {
                    window
                        .get_region_mut(name)
                        .into_iter()
                        .for_each(|r| r.clear());
                });
            window.current_tokens = window.calculate_tokens();
            compact
                .iter()
                .filter(|n| !carry.contains(n))
                .filter(|n| window.get_region(n).is_some_and(|r| !r.content.is_empty()))
                .cloned()
                .collect()
        }
    }
}

/// What `dispatch_edge_compact` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type EdgeCompactQuery = (
    Entity,
    &'static AgentState,
    &'static ContextWindow,
    &'static PendingEdgeCompact,
    Option<&'static CompactionSettings>,
);

/// Edge-compaction dispatch: for each `ReadyToInfer` agent with a
/// [`PendingEdgeCompact`] (an edge transform requested LLM summarization), spawn a
/// compaction job for the named regions (reusing the compaction lane) and hold the
/// agent `AwaitingCompaction`. If the agent has no compaction config, nothing to
/// summarize, or no provider/permit, the request is dropped and the agent proceeds
/// to inference un-compacted (memory-pressure compaction still applies later).
pub fn dispatch_edge_compact(
    mut agents: Query<EdgeCompactQuery, (With<ReadyToInfer>, Without<AwaitingCompaction>)>,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, state, window, pending, settings) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if state.status != AgentStatus::Active {
            continue; // paused / waiting / cancelled - don't start new work
        }
        let started = settings
            .and_then(|s| {
                let config = &s.0;
                let requests = build_edge_compact_requests(window, &pending.0, config)?;
                let provider = providers.0.get(&config.provider)?;
                let permit = stage.pools.try_acquire(&config.model)?;
                spawn_supervised_compaction(
                    &stage,
                    entity,
                    CompactionJob {
                        entity,
                        provider,
                        requests,
                        permit,
                    },
                );
                Some(())
            })
            .is_some();

        let mut ec = commands.entity(entity);
        ec.remove::<PendingEdgeCompact>();
        if started {
            ec.remove::<ReadyToInfer>().insert(AwaitingCompaction);
        }
    }
}

/// Build the per-region summarize requests for an edge compaction, or `None` when
/// none of the named regions have content to summarize.
pub(crate) fn build_edge_compact_requests(
    window: &ContextWindow,
    regions: &[String],
    config: &leviath_core::CompactionConfig,
) -> Option<Vec<(String, InferenceRequest)>> {
    let requests: Vec<(String, InferenceRequest)> = regions
        .iter()
        .filter_map(|name| {
            let region = window.get_region(name)?;
            let content = region
                .content
                .iter()
                .map(|e| e.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            (!content.is_empty())
                .then(|| (name.clone(), compaction_request(config, &content, name)))
        })
        .collect();
    (!requests.is_empty()).then_some(requests)
}
