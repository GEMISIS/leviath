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

/// What the `shell` tool actually runs on Windows, and the PowerShell commands
/// that stand in for the POSIX ones a model reaches for by reflex. Prepended to
/// a shell-granting stage's system blocks when [`shell_guidance_for`] returns
/// it; see [`InferenceConfig::shell_hint`](crate::components::InferenceConfig).
pub(crate) const WINDOWS_SHELL_HINT: &str = "The shell tool runs on Windows through `cmd.exe /C`, \
not a POSIX shell. GNU coreutils are not available: use `type` or PowerShell's `Get-Content` \
instead of `cat`, `findstr` or `Select-String` instead of `grep`, `dir` or `Get-ChildItem` \
instead of `ls`, and `Measure-Object -Line` instead of `wc -l`. Run a PowerShell command as \
`powershell -Command \"...\"`. Paths use backslashes and drive letters, and `%VAR%` (cmd) or \
`$env:VAR` (PowerShell) expands environment variables.";

/// The shell guidance for `os`, or `None` when the platform's shell needs no
/// explanation (a POSIX shell is what the model already assumes).
///
/// Pure over the OS string rather than `#[cfg]`-switched, following
/// `leviath_sys::browser::open_command_for`, so every branch is reachable under
/// test on a single platform. Callers pass [`std::env::consts::OS`].
pub(crate) fn shell_guidance_for(os: &str) -> Option<&'static str> {
    match os {
        "windows" => Some(WINDOWS_SHELL_HINT),
        _ => None,
    }
}

/// The framework-authored system blocks a stage carries ahead of its own
/// context, in the order they are prepended.
///
/// Both hints read the same on every agent, stage, and run of a given host, so
/// they lead the `Always`-tier prefix (which `assemble` already sorts first) and
/// leave prefix caching intact. `os` is the host OS string
/// ([`std::env::consts::OS`] in production) and `tools` the stage's advertised
/// tools: telling a stage that cannot run commands which shell it would have
/// gotten is pure overhead, so the shell hint is gated on the tool being there.
///
/// Note this is a `build_request` concern, so the request paths that assemble
/// their own [`InferenceRequest`] - `lev test`, title generation, compaction -
/// carry no hints. That was already true of the batch hint.
pub(crate) fn hint_blocks(
    config: Option<&InferenceConfig>,
    tools: &[Tool],
    os: &str,
) -> Vec<leviath_providers::SystemBlock> {
    let always = |text: &str| leviath_providers::SystemBlock {
        text: text.to_string(),
        cache_hint: leviath_core::CacheHint::Always,
    };
    let mut blocks = Vec::new();
    if config.map(|c| c.batch_tool_hint).unwrap_or(false) {
        blocks.push(always(BATCH_TOOL_HINT));
    }
    if config.map(|c| c.shell_hint).unwrap_or(false)
        && tools.iter().any(|t| t.name == "shell")
        && let Some(text) = shell_guidance_for(os)
    {
        blocks.push(always(text));
    }
    blocks
}

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

    let mut system = hint_blocks(config, &filtered_tools, std::env::consts::OS);
    system.extend(assembled.system_blocks);

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
/// inference-pool permit or tool-lane capacity the whole time.
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
            Option<&DispatchStall>,
        ),
        With<ReadyToInfer>,
    >,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    circuits: Option<Res<ProviderCircuits>>,
    policy: Option<Res<CircuitPolicy>>,
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
    let now = chrono::Utc::now().timestamp();
    let circuit_policy = policy.map(|p| *p).unwrap_or_default();
    let circuits = circuits.as_deref();
    agents.par_iter().for_each(
        |(entity, state, window, config, si, in_flight, progress, stalled)| {
            crate::tick_scope::run_agent_parallel(entity, &par_commands, &mut || {
                if state.status != AgentStatus::Active {
                    return; // paused / waiting / cancelled - don't start new work
                }
                // Every decline below records why and since when, so the
                // watchdog can tell a run that is waiting from one that is
                // waiting for something that will never happen (issue #190).
                let stall = |reason| {
                    let noted = note_stall(stalled, reason, now);
                    par_commands.command_scope(|mut commands| {
                        commands.entity(entity).insert(noted);
                    });
                };
                // The rotation system already moved this agent onto the best
                // provider still standing. Reaching a tripped one here means
                // every candidate is out of service, so park rather than send
                // a request that is going to fail the same way as the last
                // three (issue #201). The stall watchdog ends the wait.
                if circuits.is_some_and(|c| c.is_open(&si.provider_name, now, &circuit_policy)) {
                    tracing::debug!(
                        provider = %si.provider_name,
                        "inference waiting: the provider's circuit is open"
                    );
                    stall(StallReason::ProviderCircuitOpen);
                    return;
                }
                let Some(provider) = providers.0.get(&si.provider_name) else {
                    // Leave ready and retry later - but say so. A silently
                    // starved agent reads as a wedged run with no error.
                    tracing::warn!(
                        provider = %si.provider_name,
                        "inference waiting: provider not registered"
                    );
                    stall(StallReason::ProviderMissing);
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
                    stall(StallReason::PoolFull);
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
                        // Dispatched: whatever it was waiting for, it isn't
                        // waiting any more.
                        .remove::<DispatchStall>()
                        .insert(AwaitingInference);
                });
            });
        },
    );
}
