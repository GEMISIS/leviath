//! Inference dispatch: building each ready agent's request and handing it to the async lane.

use super::*;

/// The batch-tool-calls hint, prepended to a stage's system blocks when
/// `InferenceConfig::batch_tool_hint` is set. Identical across every agent,
/// stage, and run, so it is a stable cache prefix (`CacheHint::Always`). It tells
/// the model it may emit several `tool_use` blocks per response and should batch
/// *independent* operations - while explicitly forbidding batching of dependent
/// ones.
///
/// The examples name searches and fetches first because that is where the round
/// trips actually pile up: a batch is dispatched with `join_all`, so eight
/// fetches finish in about the time one does (measured: six completed inside a
/// one-second span), while the inference call between two batches costs tens of
/// seconds. A research run that spends 27 inference calls on 36 tool calls is
/// paying almost all of its wall clock for turns, not for the web. The old text
/// listed only file and shell work, so the agents doing the most fetching were
/// the ones it spoke to least.
pub(crate) const BATCH_TOOL_HINT: &str = "You can call multiple tools in a single response, \
and a batch runs in parallel rather than one after another. When operations are \
independent (searching for or fetching several different URLs, reading, editing, or \
writing different files, or writing a file then running a command that doesn't need its \
output), batch them in one response to cut round trips. A batch of eight fetches costs \
about what one costs. Do NOT batch when a call depends on a previous call's result, or \
when you must see a command's output before deciding the next step.";

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
    // `Stable`, and it matters: these blocks are the first bytes of every
    // request and never change, but the default volatility is `Rewritten`,
    // and the Anthropic breakpoint chooser reads that as "the prefix moves
    // from block zero". Measured on a research run, no request ever got its
    // stable-prefix marker for that reason alone.
    let always = |text: &str| leviath_providers::SystemBlock {
        text: text.to_string(),
        cache_hint: leviath_core::CacheHint::Always,
        volatility: leviath_core::Volatility::Stable,
        region: String::new(),
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

/// The smallest completion budget a request may carry.
///
/// Providers reject a request whose completion budget is below one - OpenAI
/// with `Invalid 'max_completion_tokens': integer below minimum value`,
/// Anthropic likewise - and the runtime derived that budget by subtracting the
/// prompt from the window with no floor under it. On a tight pinned window a
/// prompt that reached the ceiling drove it to zero and the request went out
/// anyway. A 400 does not read as transient, so the retry loop resent the same
/// doomed request until the run died (issue #495).
///
/// Deliberately one, and not something roomier. The budget is also capped at
/// what the window has left, because a provider rejects `prompt + completion`
/// past the context window just as readily - so clamping *up* to a comfortable
/// figure would trade one 400 for another. One token is the smallest request
/// the API accepts, which is the only property this constant exists to
/// guarantee; whether the reply is *useful* at that size is a budget problem,
/// and the warning beside it says so.
const MIN_OUTPUT_TOKENS: usize = 1;

/// What earlier calls in this run taught us, carried into the next request.
///
/// Three pieces of evidence with one thing in common: none of them can be
/// derived from the window as it stands, they exist only because a previous
/// request was sent and answered. The whole-prefix digest and the per-block
/// digests answer different questions ("did anything move", and "how far did it
/// hold still"); the calibration answers a third ("what did the last one really
/// cost"). They are only ever read together, so they travel together rather
/// than as adjacent parameters of similar shapes a caller could transpose.
#[derive(Debug, Clone, Default)]
pub(crate) struct PriorCalls {
    /// Digest of the whole prefix, or `None` before the first request.
    pub(crate) system_hash: Option<u64>,
    /// Per-block digests, empty before the first request.
    pub(crate) block_hashes: Vec<u64>,
    /// How far the estimate ran under what the provider charged, or `None`
    /// before anything was measured (issue #485).
    pub(crate) calibration: Option<crate::pipeline::PromptCalibration>,
    /// A reply in this stage was cut off at the output cap, so the cap goes
    /// out at the model's maximum instead of the stage's setting.
    pub(crate) raise_output_cap: bool,
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
    prior: PriorCalls,
) -> (InferenceRequest, u64, Vec<u64>) {
    let PriorCalls {
        system_hash: previous_system_hash,
        block_hashes: previous_block_hashes,
        calibration,
        raise_output_cap,
    } = prior;
    let assembled = window.assemble_with_meta(&crate::custom_region::AssembleMeta {
        stage_name: stage_name.to_string(),
        stage_iterations,
        model: stage.model.clone(),
        previous_system_hash,
        previous_block_hashes,
    });
    let system_hash = assembled.system_hash;
    let block_hashes = assembled.block_hashes.clone();
    // What the input really costs, not what the byte estimate said it would.
    // On a provider whose window is a hard ceiling the two share it, so an
    // output cap sized against an optimistic input is how a request that fit
    // when it was assembled stops fitting halfway through the answer.
    let spent = crate::pipeline::calibrated_tokens(window.current_tokens, calibration.as_ref());
    let remaining = window.max_tokens.saturating_sub(spent);
    let caps = provider.capabilities(&stage.model);
    let output_cap = match config.and_then(|c| c.max_output_tokens.as_ref()) {
        None => caps.max_output_tokens,
        Some(cap) => cap.resolve(caps.max_context_tokens, caps.max_output_tokens, |region| {
            window.get_region(region).map(|r| r.max_tokens)
        }),
    };
    // The stage's cap is what the last reply did not fit under. The model's
    // own maximum is the most room a retry can be given; a reply that does
    // not fit that either gets asked for in pieces (`cut_off_nudge`).
    let output_cap = match raise_output_cap {
        true => output_cap.max(caps.max_output_tokens),
        false => output_cap,
    };
    let max_tokens = remaining.min(output_cap).max(MIN_OUTPUT_TOKENS);
    if remaining < MIN_OUTPUT_TOKENS {
        // The prompt has filled the window and left nothing to answer with.
        // Said out loud because the request still goes out, and a reply capped
        // this short is going to be empty or truncated - which reads as a model
        // problem rather than a budget one unless somebody says so here.
        tracing::warn!(
            window_tokens = window.max_tokens,
            prompt_tokens = spent,
            "the assembled prompt leaves no room for a reply; raise the stage's \
             window or lower the region budgets that fill it"
        );
    }

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

    let request = InferenceRequest {
        system,
        messages: assembled.messages,
        model: stage.model.clone(),
        max_tokens,
        temperature,
        tools: filtered_tools,
        extra,
        request_timeout_secs: config.and_then(|c| c.request_timeout_secs),
    };
    (request, system_hash, block_hashes)
}

/// Build the [`RetryPolicy`] for a job from the operator's `[limits]` retry
/// schedule, applying a stage's per-stage inference wall-clock cap when
/// configured.
///
/// `tuning` carries the two configurable numbers (`[limits]
/// inference_retry_attempts` and `inference_retry_base_ms`); everything else -
/// the capacity schedule and the total-backoff ceiling - comes from the default
/// policy. When the stage set `request_timeout_secs` (from
/// `[stages.<name>.model]`) that overrides `job_timeout`; otherwise the default
/// job timeout stands. Pure so both branches are unit-testable without driving
/// the ECS dispatch.
pub(crate) fn retry_policy_for(
    config: Option<&InferenceConfig>,
    tuning: InferenceRetryTuning,
) -> crate::inference_bridge::RetryPolicy {
    let mut policy = crate::inference_bridge::RetryPolicy {
        max_attempts: tuning.max_attempts,
        base_delay: std::time::Duration::from_millis(tuning.base_delay_ms),
        ..crate::inference_bridge::RetryPolicy::default()
    };
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

/// What `dispatch_inference` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type InferenceQuery = (
    Entity,
    &'static AgentState,
    &'static ContextWindow,
    Option<&'static InferenceConfig>,
    &'static StageInference,
    Option<&'static InFlightWork>,
    Option<&'static StageProgress>,
    Option<&'static DispatchStall>,
    Option<&'static SystemPrefixHash>,
    Option<&'static SystemBlockHashes>,
    Option<&'static crate::pipeline::PromptCalibration>,
);

/// The system prefix the last request sent, as a digest.
///
/// Kept per agent because that is the granularity Anthropic's prefix cache
/// works at: one run's blocks, in one order. Absent before the first request,
/// which is exactly when there is nothing to invalidate.
#[derive(bevy_ecs::component::Component, Debug, Clone, Copy)]
pub struct SystemPrefixHash(pub u64);

/// The previous request's per-block system digests, kept on the agent so the
/// next assembly can tell which blocks held still and place cache breakpoints
/// only where the entry is readable back (issue #474).
#[derive(Component, Debug, Clone, Default)]
pub struct SystemBlockHashes(pub Vec<u64>);

/// Inference-dispatch system: for every `ReadyToInfer` agent, resolve its
/// provider and, **if a per-model permit is free**, build the request, spawn the
/// inference job, and move it to `AwaitingInference`. If its provider is missing
/// or no slot is free, it stays `ReadyToInfer` and is retried on a later tick -
/// no blocking, no wasted task.
pub fn dispatch_inference(
    agents: Query<InferenceQuery, With<ReadyToInfer>>,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    circuits: Option<Res<ProviderCircuits>>,
    policy: Option<Res<CircuitPolicy>>,
    retry: Option<Res<InferenceRetryTuning>>,
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
    // The daemon inserts this from `[limits]`; a world that never set it (every
    // embedded host, and most tests) gets the built-in schedule.
    let retry_tuning = retry.map(|r| *r).unwrap_or_default();
    let circuits = circuits.as_deref();
    agents.par_iter().for_each(
        |(
            entity,
            state,
            window,
            config,
            si,
            in_flight,
            progress,
            stalled,
            prefix,
            block_prefix,
            calibration,
        )| {
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
                let Some(permit) = stage.pools.try_acquire(&si.provider_name, &si.model) else {
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
                let (request, system_hash, block_hashes) = build_request(
                    window,
                    config,
                    si,
                    &provider,
                    &state.current_stage,
                    progress.map(|p| p.iterations).unwrap_or(0),
                    PriorCalls {
                        system_hash: prefix.map(|p| p.0),
                        block_hashes: block_prefix.map(|b| b.0.clone()).unwrap_or_default(),
                        calibration: calibration.copied(),
                        raise_output_cap: progress.is_some_and(|p| p.raise_output_cap),
                    },
                );
                // Remembered for the next request, which is the only way the
                // breakpoint decision can be made on evidence.
                par_commands.command_scope(|mut commands| {
                    commands
                        .entity(entity)
                        .insert(SystemPrefixHash(system_hash));
                    commands
                        .entity(entity)
                        .insert(SystemBlockHashes(block_hashes.clone()));
                    // What the window believes this call will cost. The response
                    // says what it really cost, and the two together are the
                    // only measurement of the estimator's drift the runtime
                    // gets (issue #485).
                    commands
                        .entity(entity)
                        .insert(crate::pipeline::PromptEstimate(window.current_tokens));
                });
                // A provider that does not advertise streaming for this model
                // is called the old way whatever the config says: `infer_stream`
                // has a default that buffers and then emits one chunk, so
                // asking anyway would pay for the fold and gain nothing.
                let stream =
                    stage.stream_inference && provider.capabilities(&si.model).supports_streaming;
                let job = InferenceJob {
                    entity,
                    provider,
                    request,
                    permit,
                    exact_token_counting: stage.exact_token_counting,
                    stream,
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
                        retry_policy_for(config, retry_tuning),
                        cancel.clone(),
                    ),
                    move |message| {
                        let _ = lost_outcomes.send(InferenceOutcome {
                            entity,
                            result: Err(leviath_providers::ProviderError::Other(message)),
                            // The job never got to measure itself.
                            latency: std::time::Duration::ZERO,
                            // ...and never reached a provider, so it billed
                            // nothing and needs no rates.
                            pricing: None,
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
