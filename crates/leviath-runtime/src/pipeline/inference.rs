//! Inference dispatch: building each ready agent's request and handing it to the async lane.

use super::*;

/// The batch-tool-calls hint, prepended to a stage's system blocks when
/// `InferenceConfig::batch_tool_hint` is set. Identical across every agent,
/// stage, and run, so it is a stable cache prefix (`CacheHint::Always`). It tells
/// the model it may emit several `tool_use` blocks per response and should batch
/// *independent* operations - while explicitly forbidding batching of dependent
/// ones.
pub(crate) const BATCH_TOOL_HINT: &str = "You can call multiple tools in a single response. \
When operations are independent (reading, editing, or writing different files, or \
writing a file then running a command that doesn't need its output), batch them in \
one response to cut round trips. Do NOT batch when a call depends on a previous \
call's result, or when you must see a command's output before deciding the next step.";

/// Build the [`InferenceRequest`] for an agent from its context window + stage
/// data. Pure; no `.await` - a custom region's render hook is a bounded,
/// synchronous Rhai eval. (Ported from `AgentEngine::build_inference_request`,
/// with provider resolution lifted into the caller so this stays query-friendly.)
///
/// `stage_name` / `stage_iterations` feed custom-region `render(ctx)` hooks;
/// they change nothing when the window has no custom regions.
pub(crate) fn build_request(
    window: &ContextWindow,
    config: Option<&InferenceConfig>,
    stage: &StageInference,
    provider: &Arc<dyn Provider>,
    stage_name: &str,
    stage_iterations: usize,
) -> InferenceRequest {
    let assembled = window.assemble_with_meta(&crate::custom_region::AssembleMeta {
        stage_name: stage_name.to_string(),
        stage_iterations,
        model: stage.model.clone(),
    });
    let remaining = window.max_tokens.saturating_sub(window.current_tokens);
    let caps = provider.capabilities(&stage.model);
    let output_cap = config
        .and_then(|c| c.max_output_tokens)
        .unwrap_or(caps.max_output_tokens);
    let max_tokens = remaining.min(output_cap);

    let filtered_tools = match stage.tool_filter.as_deref() {
        Some(filter) if !filter.is_empty() => stage
            .tools
            .iter()
            .filter(|t| filter.iter().any(|f| f == &t.name))
            .cloned()
            .collect(),
        _ => stage.tools.clone(),
    };

    let temperature = if caps.supports_temperature {
        config.and_then(|c| c.temperature).unwrap_or(0.7)
    } else {
        0.0
    };

    // Pass through any extra model parameters (top_p, stop, seed, …) so the
    // provider can apply them; `Null` when there are none.
    let extra = match config.map(|c| &c.extra_params) {
        Some(params) if !params.is_empty() => serde_json::Value::Object(params.clone()),
        _ => serde_json::Value::Null,
    };

    // Prepend the batch-tool-calls hint as a stable, always-cacheable system
    // block when this stage opts in. Prepending keeps it at the front of the
    // `Always`-tier prefix (which `assemble` already sorts first), maximizing
    // prefix-cache hits since the text never varies across agents/stages/runs.
    let mut system = assembled.system_blocks;
    if config.map(|c| c.batch_tool_hint).unwrap_or(false) {
        system.insert(
            0,
            leviath_providers::SystemBlock {
                text: BATCH_TOOL_HINT.to_string(),
                cache_hint: leviath_core::CacheHint::Always,
            },
        );
    }

    InferenceRequest {
        system,
        messages: assembled.messages,
        model: stage.model.clone(),
        max_tokens,
        temperature,
        tools: filtered_tools,
        extra,
        request_timeout_secs: config.and_then(|c| c.request_timeout_secs),
    }
}

/// Build the [`RetryPolicy`] for a job, applying a stage's per-stage inference
/// wall-clock cap when configured. Starts from the default policy and, when the
/// stage set `request_timeout_secs` (from `[stages.<name>.model]`), overrides its
/// `job_timeout`; otherwise the default job timeout stands. Pure so the override
/// branch is unit-testable without driving the ECS dispatch.
pub(crate) fn retry_policy_for(
    config: Option<&InferenceConfig>,
) -> crate::inference_bridge::RetryPolicy {
    let mut policy = crate::inference_bridge::RetryPolicy::default();
    if let Some(secs) = config.and_then(|c| c.request_timeout_secs) {
        policy.job_timeout = std::time::Duration::from_secs(secs);
    }
    policy
}

/// The cancellation handles for an agent's currently in-flight async work (its
/// inference request, its tool batch). Attached when the work is dispatched,
/// removed when it lands - so the presence of this component means "there is
/// something running for this agent that a cancel needs to stop".
///
/// Without it, cancelling only stopped *new* work from being dispatched: a
/// request already handed to the async lanes ran to completion, holding its
/// inference-pool permit or tool-lane worker the whole time.
#[derive(Component, Default, Debug)]
pub struct InFlightWork(pub Vec<crate::cancel::CancelToken>);

/// Stop the in-flight work of every agent that has reached a terminal state, and
/// drop the handles. Runs before the dispatch systems each tick, so a cancel
/// takes effect on the very next tick rather than whenever the provider or tool
/// happens to answer.
pub fn abort_terminal_work(
    agents: Query<(Entity, &AgentState, &InFlightWork)>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, state, in_flight) in agents.iter() {
        if !is_terminal_status(&state.status) {
            continue;
        }
        crate::tick_scope::enter(entity);
        for token in &in_flight.0 {
            token.cancel();
        }
        commands.entity(entity).remove::<InFlightWork>();
    }
}

/// Record `token` as in-flight work for `entity`, keeping any already attached
/// (an agent can have both a tool batch and an inference outstanding across a
/// tick boundary).
pub(crate) fn track_in_flight(
    commands: &mut Commands,
    entity: Entity,
    existing: Option<&InFlightWork>,
    token: crate::cancel::CancelToken,
) {
    let mut tokens = existing.map(|w| w.0.clone()).unwrap_or_default();
    tokens.push(token);
    commands.entity(entity).insert(InFlightWork(tokens));
}

/// Inference-dispatch system: for every `ReadyToInfer` agent, resolve its
/// provider and, **if a per-model permit is free**, build the request, spawn the
/// inference job, and move it to `AwaitingInference`. If its provider is missing
/// or no slot is free, it stays `ReadyToInfer` and is retried on a later tick -
/// no blocking, no wasted task.
#[allow(clippy::type_complexity)]
pub fn dispatch_inference(
    agents: Query<
        (
            Entity,
            &AgentState,
            &ContextWindow,
            Option<&InferenceConfig>,
            &StageInference,
            Option<&InFlightWork>,
            Option<&StageProgress>,
        ),
        With<ReadyToInfer>,
    >,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    par_commands: ParallelCommands,
) {
    // Fan out across ready agents: request assembly (`build_request`) is the
    // per-agent CPU cost and is independent, so it runs in parallel on the
    // compute pool. Permit acquisition (an atomic semaphore) and the tokio spawn
    // are thread-safe; the marker swap is batched via `ParallelCommands`.
    //
    // This is the one system whose per-agent body runs off the driver thread, so
    // the thread-local `tick_scope` can't carry an entity back to the catcher.
    // Each agent's share runs under `run_agent_parallel`, which catches there -
    // where the entity is known - and marks that agent for `tick` to fail
    // (issue #109). Clearing the thread-local keeps a panic in the fan-out
    // machinery *itself* unattributed rather than blamed on whichever agent a
    // previous system left recorded.
    crate::tick_scope::clear();
    agents
        .par_iter()
        .for_each(|(entity, state, window, config, si, in_flight, progress)| {
            crate::tick_scope::run_agent_parallel(entity, &par_commands, &mut || {
                if state.status != AgentStatus::Active {
                    return; // paused / waiting / cancelled - don't start new work
                }
                let Some(provider) = providers.0.get(&si.provider_name) else {
                    // Leave ready and retry later - but say so. A silently
                    // starved agent reads as a wedged run with no error.
                    tracing::warn!(
                        provider = %si.provider_name,
                        "inference waiting: provider not registered"
                    );
                    return;
                };
                let Some(permit) = stage.pools.try_acquire(&si.model) else {
                    // Every in-flight call on this model holds a permit; if
                    // this repeats for minutes, one of them is stuck (see the
                    // default request timeout in leviath-providers).
                    tracing::debug!(
                        model = %si.model,
                        "inference waiting: per-model pool is full"
                    );
                    return;
                };
                let request = build_request(
                    window,
                    config,
                    si,
                    &provider,
                    &state.current_stage,
                    progress.map(|p| p.iterations).unwrap_or(0),
                );
                let job = InferenceJob {
                    entity,
                    provider,
                    request,
                    permit,
                    exact_token_counting: stage.exact_token_counting,
                };
                let cancel = crate::cancel::CancelToken::new();
                // Supervised: this agent is about to become `AwaitingInference`,
                // which the driver reads as "busy". A job that died without
                // reporting would leave it waiting on a completion that can no
                // longer come, so the supervisor reports one in its place.
                let lost_outcomes = stage.outcomes.clone();
                let lost_wake = stage.wake.clone();
                crate::lane_supervisor::spawn_supervised(
                    &stage.runtime,
                    "inference",
                    run_inference_job(
                        job,
                        stage.outcomes.clone(),
                        stage.wake.clone(),
                        retry_policy_for(config),
                        cancel.clone(),
                    ),
                    move |message| {
                        let _ = lost_outcomes.send(InferenceOutcome {
                            entity,
                            result: Err(leviath_providers::ProviderError::Other(message)),
                            // The job never got to measure itself.
                            latency: std::time::Duration::ZERO,
                        });
                        lost_wake.notify_one();
                    },
                );
                par_commands.command_scope(|mut commands| {
                    track_in_flight(&mut commands, entity, in_flight, cancel);
                    commands
                        .entity(entity)
                        .remove::<ReadyToInfer>()
                        .insert(AwaitingInference);
                });
            });
        });
}
