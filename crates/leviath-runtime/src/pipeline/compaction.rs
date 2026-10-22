//! Threshold compaction and edge-transform compaction, over the compaction lane.

use super::*;

// ─── Compaction (LLM context summarization) ──────────────────────────────────

/// Per-agent compaction configuration: the model that summarizes compacting
/// regions before an inference. Without it those regions are not summarized;
/// a full window still makes room by eviction either way.
#[derive(Component, Clone)]
pub struct CompactionSettings(pub leviath_core::CompactionConfig);

/// A compaction job (LLM summarization) is in flight; the agent is held out of
/// inference until its summaries land.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AwaitingCompaction;

/// The receiving end of the compaction-outcomes channel, as a world resource.
/// (The sending end lives in [`InferenceStage::compaction_outcomes`].)
#[derive(Resource)]
pub(crate) struct CompactionResults(pub UnboundedReceiver<CompactionOutcome>);

/// The eviction threshold (fraction of budget) at which compaction kicks in.
pub(crate) const EVICTION_THRESHOLD: f32 = 0.9;

/// Every route a summary for this agent may take now, best first: the
/// graph's compaction routes, or, when it names none, the model the current
/// stage runs on followed by that stage's fallbacks.
///
/// A route is left out when its provider is not registered, or when zero
/// retention refuses its model: a summary carries the run's context, and the
/// spawn gate refuses or drops such a model, so meeting one here means the
/// switch was turned on under a running daemon. Each route carries the
/// request fields its provider's retention settings add.
pub(crate) fn summary_routes(
    config: &leviath_core::CompactionConfig,
    stage: Option<&StageInference>,
    registry: &ProviderRegistry,
) -> Vec<SummaryRoute> {
    let named: Vec<(String, String)> = match config.routes.is_empty() {
        false => config
            .routes
            .iter()
            .map(|r| (r.provider_or_empty().to_string(), r.model.to_string()))
            .collect(),
        true => stage
            .into_iter()
            .flat_map(|si| {
                std::iter::once((si.provider_name.clone(), si.model.clone())).chain(
                    si.fallbacks
                        .iter()
                        .map(|f| (f.provider_or_empty().to_string(), f.model.to_string())),
                )
            })
            .collect(),
    };
    named
        .into_iter()
        .filter_map(|(provider_name, model)| {
            let provider = registry.get(&provider_name)?;
            let refused = registry.retention_refusal(&provider_name, &model).is_some();
            let mut extra = serde_json::Value::Null;
            registry.apply_retention_knobs(&provider_name, &mut extra);
            (!refused).then_some(SummaryRoute {
                provider,
                provider_name,
                model,
                extra,
            })
        })
        .collect()
}

/// Why a summary did not start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Declined {
    /// No route can take it here.
    NoRoute,
    /// Every route that can is at its pool's limit this round.
    PoolFull,
}

/// Start summarizing `requests` for `entity` on the first of `routes` with a
/// free pool slot, the rest kept for failover, reporting into `outcomes`
/// under the lane supervisor (see [`spawn_summary_job`]).
pub(crate) fn start_summary(
    stage: &InferenceStage,
    outcomes: &tokio::sync::mpsc::UnboundedSender<CompactionOutcome>,
    lane: &'static str,
    entity: Entity,
    routes: Vec<SummaryRoute>,
    requests: Vec<(String, InferenceRequest)>,
) -> Result<(), Declined> {
    if routes.is_empty() {
        return Err(Declined::NoRoute);
    }
    let mut rest = routes.into_iter();
    let (route, permit) = rest
        .by_ref()
        .find_map(|r| {
            stage
                .pools
                .try_acquire(&r.provider_name, &r.model)
                .map(|p| (r, p))
        })
        .ok_or(Declined::PoolFull)?;
    spawn_summary_job(
        stage,
        outcomes,
        lane,
        entity,
        CompactionJob {
            entity,
            route,
            permit,
            fallbacks: rest.collect(),
            pools: stage.pools.clone(),
            requests,
        },
    );
    Ok(())
}

/// Spawn a summarization job under the lane supervisor, reporting into
/// `outcomes`: compaction's own lane, or the content-summary lane a child's
/// summarized region waits on. A job that dies without reporting is reported
/// as an error named after `lane`, which each lane's collect system already
/// treats as a failed summary.
pub(crate) fn spawn_summary_job(
    stage: &InferenceStage,
    outcomes: &tokio::sync::mpsc::UnboundedSender<CompactionOutcome>,
    lane: &'static str,
    entity: Entity,
    job: CompactionJob,
) {
    let lost_outcomes = outcomes.clone();
    let lost_wake = stage.wake.clone();
    crate::lane_supervisor::spawn_supervised(
        &stage.runtime,
        lane,
        run_compaction_job(
            job,
            std::time::Duration::from_secs(leviath_providers::DEFAULT_INFERENCE_TIMEOUT_SECS),
            outcomes.clone(),
            stage.wake.clone(),
        ),
        move |message| {
            let _ = lost_outcomes.send(CompactionOutcome {
                entity,
                result: Err(leviath_providers::ProviderError::Other(message)),
                // Nothing ran, so there is nothing to price.
                pricing: None,
                // A job that never ran billed nothing. Empty rather than
                // absent: there is no call to attribute, not an unknown cost.
                usage: Vec::new(),
                provider_name: String::new(),
                model: String::new(),
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
    Option<&'static CompactionSettings>,
    Option<&'static crate::pipeline::PromptCalibration>,
    Option<&'static StageInference>,
);

/// Compaction-dispatch system: for each `ReadyToInfer` agent, when the window
/// as a whole is over the eviction threshold, run the synchronous eviction;
/// then, for an agent with [`CompactionSettings`], summarize every compacting
/// region that is past its own threshold (`compact_at`). Builds one request
/// per region with content to summarize, starts the job on the first of its
/// routes ([`summary_routes`]) with a free slot, and holds the agent as
/// `AwaitingCompaction`. Anything that can't proceed (nothing past a
/// threshold, nothing to summarize, no route this machine can serve, every
/// route's pool full) simply leaves the agent `ReadyToInfer` so inference
/// proceeds - compaction is best-effort.
///
/// A region's own threshold is what `compact_at` promises, and acting on it
/// before the window fills is what keeps a compacting region summarizing
/// rather than rolling its oldest entries off when a write does not fit.
/// Without window pressure a region holding a single entry waits: summarizing
/// one entry in place leaves one entry, and a summary still past the threshold
/// would be summarized again before every request.
pub(crate) fn dispatch_compaction(
    mut agents: Query<CompactionQuery, (With<ReadyToInfer>, Without<AwaitingCompaction>)>,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, state, mut window, settings, calibration, inference) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if state.status != AgentStatus::Active {
            continue; // paused / waiting / cancelled - don't start new work
        }
        // Against the corrected estimate, not the raw one. The threshold is
        // there to leave room between "nearly full" and "over the window", and
        // an estimate measured running light spends that room without ever
        // reporting it.
        let pressed = crate::pipeline::needs_eviction_calibrated(
            window.current_tokens,
            window.max_tokens,
            EVICTION_THRESHOLD,
            calibration,
        );
        let target_free = window.max_tokens / 10;
        if pressed && window.try_evict(target_free).is_err() {
            continue; // couldn't evict - proceed to inference as-is
        }
        // Summarizing needs a compaction model. Without one, the eviction
        // above is all the room a full window gets.
        let Some(CompactionSettings(config)) = settings else {
            continue;
        };

        // Build a summarize request per region that is past its threshold and
        // has content to summarize.
        let mut requests = Vec::new();
        for region in window
            .regions
            .iter()
            .filter(|r| r.needs_compaction() && (pressed || r.content.len() > 1))
        {
            let region_name = &region.name;
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

        // Every route the summary may take, best first. A route that cannot
        // take it (provider gone, zero retention refusing it) is left out, so
        // the summary is only skipped when no route can serve it at all, or
        // every one of them is busy this round.
        let routes = summary_routes(config, inference, &providers.0);
        let started = start_summary(
            &stage,
            &stage.compaction_outcomes,
            "compaction",
            entity,
            routes,
            requests,
        );
        if started.is_ok() {
            commands
                .entity(entity)
                .remove::<ReadyToInfer>()
                .insert(AwaitingCompaction);
        }
    }
}

/// What `collect_compaction` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type CollectCompactionQuery = (
    &'static mut ContextWindow,
    Option<&'static mut crate::telemetry::StageActivity>,
    Option<&'static mut crate::persistence::TokenTotals>,
    Option<&'static crate::persistence::RunMetadata>,
    Option<&'static AgentState>,
    Option<&'static mut crate::pipeline::StageLedger>,
    Option<&'static crate::continuation::ContinuationCompact>,
);

/// Compaction-collect system: drain finished compaction jobs and apply each
/// summary into its paired `CompactHistory` region, clearing the summarized
/// source region. A provider error leaves the context untouched (best-effort),
/// except in a region a continuation asked to summarize, which is emptied.
/// Either way the agent returns to `ReadyToInfer`.
pub(crate) fn collect_compaction(
    mut results: ResMut<CompactionResults>,
    mut agents: Query<CollectCompactionQuery, With<AwaitingCompaction>>,
    persist: Option<Res<crate::pipeline::JournalSender>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    while let Ok(outcome) = results.0.try_recv() {
        let Ok((mut window, activity, mut totals, md, state, mut ledger, continuing)) =
            agents.get_mut(outcome.entity)
        else {
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
        // One record per call, not one per batch. A batch summarizes a region
        // each, so folding them into a single number would recreate downstream
        // exactly the ambiguity this record exists to remove.
        //
        // Counted even when the batch failed partway: the calls that already
        // ran were billed, and the summaries being discarded is a decision
        // about the window, not about the invoice.
        // Billed to the stage the run was in when the window filled up, the same
        // way its own turns are. Compaction is not free and not incidental - a
        // stage that compacts twice can spend more on summarizing its context
        // than on the work - so leaving it out of the ledger would leave the
        // one question the ledger exists to answer, which stage cost that,
        // answerable only for the cheap half of the bill.
        for usage in &outcome.usage {
            crate::inference_usage::record_call(
                totals.as_deref_mut(),
                ledger.as_deref_mut(),
                persist.as_deref(),
                md,
                &crate::inference_usage::CallUsage {
                    kind: crate::runfile::record::InferenceKind::Compaction,
                    stage: state.map_or("", |s| s.current_stage.as_str()),
                    iteration: state.map_or(0, |s| s.iteration),
                    provider: &outcome.provider_name,
                    model: &outcome.model,
                    usage,
                    pricing: outcome.pricing,
                },
            );
        }
        let mut summarized: Vec<String> = Vec::new();
        if let Ok(summaries) = outcome.result {
            for (region_name, summary) in summaries {
                // A summary with nothing in it is a compaction that failed, not
                // one that found nothing worth keeping - and writing it would
                // trade the region's real contents for a blank. Measured on a
                // 32k window, where the transcript being summarized was small
                // enough that the model returned an empty string; a stored
                // blank reaches a provider as a zero-length turn, which is a
                // 400. Leave the region as written and
                // say so: eviction has other phases, and losing the content is
                // worse than staying over budget for another tick.
                if summary.trim().is_empty() {
                    tracing::warn!(
                        region = %region_name,
                        "compaction returned an empty summary; keeping the region \
                         as written rather than replacing it with nothing"
                    );
                    continue;
                }
                // The summary is text: whatever stored parts the region held
                // are gone with the entries it replaces. Said once, by name,
                // so a run that lost an image to compaction can tell why the
                // model stopped seeing it; the stand-ins the summary was
                // written from are what it keeps.
                let dropped: Vec<String> = window
                    .get_region(&region_name)
                    .map(|r| {
                        r.content
                            .iter()
                            .flat_map(|e| e.content.stored())
                            .map(|p| p.name.clone().unwrap_or_else(|| "(unnamed)".to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                if !dropped.is_empty() {
                    let names = dropped.join(", ");
                    tracing::warn!(
                        region = %region_name,
                        parts = %names,
                        "[mime] compaction replaced entries carrying stored parts with a text summary; \
                         the parts stay in the run's store but leave the window"
                    );
                }
                let summary_tokens = leviath_core::estimate_tokens(&summary);
                let history = window
                    .regions
                    .iter()
                    .find(|r| {
                        matches!(&r.kind, leviath_core::RegionKind::CompactHistory { source_region }
                            if source_region == &region_name)
                    })
                    .map(|r| r.name.clone());
                // The summary rolls forward into the region's `compact_history`
                // when it has one, and otherwise stays in the region it
                // summarizes: a compacting region summarizes instead of
                // evicting, so its older content is never simply dropped.
                let into = history.unwrap_or_else(|| region_name.clone());
                let before = window.begin_change(&region_name);
                if let Some(region) = window.get_region_mut(&region_name) {
                    region.clear();
                }
                // The source region emptying is half of what a compaction did,
                // and the half a reader is most likely to be looking for.
                window.commit_change(
                    leviath_core::ContextCause::Compaction,
                    before,
                    crate::components::Pushed::Nothing,
                );
                let _ = window.add_to_region_caused(
                    leviath_core::ContextCause::Compaction,
                    &into,
                    summary,
                    summary_tokens,
                );
                summarized.push(region_name);
            }
            window.current_tokens = window.calculate_tokens();
        }
        // A continuation's regions are replaced whatever happened: the ones
        // with no summary to show for it are emptied (see
        // `ContinuationCompact`).
        if let Some(continuing) = continuing.filter(|c| c.started) {
            let unsummarized: Vec<String> = continuing
                .regions
                .iter()
                .filter(|name| !summarized.contains(name))
                .cloned()
                .collect();
            crate::continuation::empty_unsummarized(&mut window, &unsummarized);
            commands
                .entity(outcome.entity)
                .remove::<crate::continuation::ContinuationCompact>();
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
                reasoning: None,
            },
            leviath_providers::Message {
                role: "user".to_string(),
                content: config.user_prompt(content, region_name).into(),
                cache_breakpoint: false,
                reasoning: None,
            },
        ],
        // Named per route when the request is sent.
        model: String::new(),
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
pub(crate) struct PendingEdgeCompact(pub Vec<String>);

/// Whether a region kind is "stage-specific" - eligible for an edge transform to
/// clear or compact. The always-preserved kinds (pinned identity, compaction
/// history, hashmap stores, persistent custom regions) are never touched.
pub fn is_stage_specific(kind: &leviath_core::RegionKind) -> bool {
    !matches!(
        kind,
        leviath_core::RegionKind::Pinned
            | leviath_core::RegionKind::CompactHistory { .. }
            | leviath_core::RegionKind::HashMap { .. }
            | leviath_core::RegionKind::Custom { pinned: true, .. }
    )
}

/// Apply an edge transform's **synchronous** effects to the outgoing window
/// (clearing stage-specific / named regions) and return the names of regions the
/// caller should hand to the LLM compaction lane. `Direct` on a linear or
/// chosen edge carries context as it is.
pub(crate) fn apply_edge_transform(
    window: &mut ContextWindow,
    transform: &crate::spec::graph::EdgeCarry,
) -> Vec<String> {
    use crate::spec::graph::EdgeCarry;
    match transform {
        EdgeCarry::Direct => Vec::new(),
        EdgeCarry::Clear => {
            window
                .regions
                .iter_mut()
                .filter(|r| is_stage_specific(&r.kind))
                .for_each(|r| r.clear());
            window.current_tokens = window.calculate_tokens();
            Vec::new()
        }
        // Kind cannot tell a transcript from a table of results, so a region
        // whose author said its content does not survive a paraphrase is left
        // alone however the edge is spelled.
        EdgeCarry::Compact { .. } => window
            .regions
            .iter()
            .filter(|r| is_stage_specific(&r.kind) && r.summarizable && !r.content.is_empty())
            .map(|r| r.name.clone())
            .collect(),
        EdgeCarry::Custom {
            carry,
            compact,
            clear,
            ..
        } => {
            // One transaction over every region the edge clears. The edge clears
            // them as one act, and recording each on its own would leave a
            // reader to guess from the timestamps which of them went together.
            let cleared: Vec<&str> = clear
                .iter()
                .filter(|n| !carry.contains(n))
                .map(|n| n.as_str())
                .collect();
            let emptying = window.begin_changes(cleared.iter().copied());
            for name in &cleared {
                window
                    .get_region_mut(name)
                    .into_iter()
                    .for_each(|r| r.clear());
            }
            window.current_tokens = window.calculate_tokens();
            window.commit_change(
                leviath_core::ContextCause::Transform,
                emptying,
                crate::components::Pushed::Nothing,
            );
            compact
                .iter()
                .filter(|n| !carry.contains(n))
                .filter(|n| {
                    // The region-level flag wins over an explicit list: it is
                    // there so a deliverable is protected wherever it is used,
                    // rather than at each of the N edges that might touch it.
                    // Said out loud, because refusing an explicit instruction
                    // silently is the thing this issue is about.
                    match window.get_region(n.as_str()) {
                        Some(r) if !r.summarizable => {
                            tracing::warn!(
                                region = %n,
                                "edge asks to compact a region declared \
                                 summarizable = false; leaving it as written"
                            );
                            false
                        }
                        Some(r) => !r.content.is_empty(),
                        None => false,
                    }
                })
                .map(ToString::to_string)
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
    &'static mut ContextWindow,
    &'static PendingEdgeCompact,
    Option<&'static CompactionSettings>,
    Option<&'static StageInference>,
    Option<&'static mut crate::continuation::ContinuationCompact>,
);

/// Edge-compaction dispatch: for each `ReadyToInfer` agent with a
/// [`PendingEdgeCompact`] (an edge transform requested LLM summarization), spawn a
/// compaction job for the named regions (reusing the compaction lane) and hold the
/// agent `AwaitingCompaction`. If the agent has no compaction config, nothing to
/// summarize, or no route with a free slot, the request is dropped and the agent proceeds
/// to inference un-compacted (memory-pressure compaction still applies later).
/// A continuation's request is the exception: it waits for a free slot, and
/// with no route its regions are emptied rather than carried.
pub(crate) fn dispatch_edge_compact(
    mut agents: Query<EdgeCompactQuery, (With<ReadyToInfer>, Without<AwaitingCompaction>)>,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, state, mut window, pending, settings, inference, continuing) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        if state.status != AgentStatus::Active {
            continue; // paused / waiting / cancelled - don't start new work
        }
        // `None` when there is nothing to start: no compaction settings, or
        // nothing in the named regions to summarize.
        let outcome = settings.and_then(|s| {
            let config = &s.0;
            let requests = build_edge_compact_requests(&window, &pending.0, config)?;
            let routes = summary_routes(config, inference, &providers.0);
            Some(start_summary(
                &stage,
                &stage.compaction_outcomes,
                "compaction",
                entity,
                routes,
                requests,
            ))
        });
        let mut ec = commands.entity(entity);
        if let Some(mut continuing) = continuing {
            // A continuation's summary is not best effort; see
            // `ContinuationCompact`.
            match outcome {
                Some(Ok(())) => continuing.started = true,
                // Asked again next tick, the message waiting behind it.
                Some(Err(Declined::PoolFull)) => continue,
                _ => {
                    crate::continuation::empty_unsummarized(&mut window, &continuing.regions);
                    ec.remove::<crate::continuation::ContinuationCompact>();
                }
            }
        } else {
            // Each way this can decline says which one it was. A declared
            // transform that quietly does nothing is a transform that behaves
            // differently on different runs of the same blueprint, with no
            // signal either way - and the un-compacted run looks identical to
            // a compacted one from outside.
            match outcome {
                Some(Err(Declined::NoRoute)) => tracing::warn!(
                    regions = ?pending.0,
                    "edge transform asked to compact, but no compaction route can \
                     take it here (no provider for one is registered, or zero \
                     retention refuses each); carrying the regions as written"
                ),
                Some(Err(Declined::PoolFull)) => tracing::warn!(
                    regions = ?pending.0,
                    "edge transform asked to compact, but every compaction route's \
                     pool is full; carrying the regions as written"
                ),
                _ => {}
            }
        }
        ec.remove::<PendingEdgeCompact>();
        if matches!(outcome, Some(Ok(()))) {
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

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod tests;
