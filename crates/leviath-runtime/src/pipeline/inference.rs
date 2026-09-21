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
/// Anthropic likewise - and the budget is derived by subtracting the prompt
/// from the window. With no floor under it, a tight pinned window whose prompt
/// reaches the ceiling drives it to zero and the request goes out anyway. A 400
/// does not read as transient, so the retry loop resends the same doomed
/// request until the run dies.
///
/// Deliberately one, and not something roomier. The budget is also capped at
/// what the window has left, because a provider rejects `prompt + completion`
/// past the context window just as readily - so clamping *up* to a comfortable
/// figure would trade one 400 for another. One token is the smallest request
/// the API accepts, which is the only property this constant exists to
/// guarantee; whether the reply is *useful* at that size is a budget problem,
/// and the warning beside it says so.
const MIN_OUTPUT_TOKENS: usize = 1;

/// The share of the prompt held back from the answer when the window, and not
/// the model's own cap, is what limits the reply. One sixteenth.
///
/// The window is shared between the prompt and the answer, and only one of the
/// two is a measurement: the prompt is [`leviath_core::estimate_tokens`], bytes
/// over four, while the provider counts with its own tokenizer and counts the
/// tool schemas and its own message framing besides.
/// [`PromptCalibration`](crate::pipeline::PromptCalibration) corrects that from
/// what earlier calls were charged, but it can only correct by what it has
/// already seen, and a stage's first call carries schemas nothing has measured
/// yet. Asking for every token the estimate says is left means any remaining
/// light spot is not a shorter answer but a rejected request.
///
/// This is not the guessed margin the calibration module argues against. That
/// one would hold context back from every workload; this one only ever shortens
/// a reply, only when the window is the binding constraint - where the model's
/// cap is the smaller number the `min` below already leaves the difference as
/// slack - and it is proportional because the error it covers is.
const PROMPT_ESTIMATE_HEADROOM: usize = 16;

/// The tokens a provider will bill for the stored parts a request sends as
/// bytes, over what the window charged for them.
///
/// The window charges a stored part its one-line stand-in; a model that takes
/// the bytes is billed the part's real cost, which the registry estimated at
/// ingest. Only the parts this model takes count, and not one the stage sends
/// as text (`as_text`), which is billed as the text it is. Hydration may still
/// cap or drop a part, in which case the bill comes in under this figure and
/// the calibration simply sees no shortfall - the safe direction to miss in.
pub(super) fn native_media_tokens(
    request: &leviath_providers::InferenceRequest,
    mime: &leviath_providers::capabilities::ModelMime,
    as_text: &[String],
) -> usize {
    request
        .messages
        .iter()
        .filter_map(|m| match &m.content {
            leviath_providers::MessageContent::Blocks(blocks) => Some(blocks),
            leviath_providers::MessageContent::Text(_) => None,
        })
        .flatten()
        .filter_map(|block| match block {
            leviath_providers::ContentBlock::Mime { part, deliver, .. }
                if mime.accepts(&part.mime_type)
                    && *deliver != Some(leviath_core::mime::Delivery::Text)
                    && !part.mime_type.matches_any(as_text) =>
            {
                Some(
                    part.tokens
                        .saturating_sub(leviath_core::estimate_tokens(&part.stand_in)),
                )
            }
            _ => None,
        })
        .sum()
}

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
    /// before anything was measured.
    pub(crate) calibration: Option<crate::pipeline::PromptCalibration>,
    /// A reply in this stage was cut off at the output cap, so the cap goes
    /// out at the model's maximum instead of the stage's setting.
    pub(crate) raise_output_cap: bool,
}

/// This run writes the exact request it sends the model into its journal, once
/// per provider attempt.
///
/// A marker rather than a field on the stage's inference config, because it is a
/// property of the run and not of a stage: it arrives from `[observability]
/// capture_model_input` or from the spawn's own `capture_model_input`, and every
/// stage of a captured run is captured.
///
/// Absent on all but the runs whose operator asked, which is the whole safety
/// property: a captured request is the whole prompt, with whatever the context
/// held in it.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct CaptureModelInput;

/// The version of the prompt-assembly logic, recorded beside every captured
/// request so a body stays interpretable once assembly changes.
///
/// Bumped by hand when what a request *means* changes: a system block that moves
/// tier, a message shape that is built differently, guidance that is prepended
/// where it was not. Adding a field a provider ignores does not move it.
pub const MODEL_INPUT_ASSEMBLY_VERSION: &str = "1";

/// An opaque identifier for the tool set one request offered the model.
///
/// Folded from each tool's name, description and parameter schema, so two
/// requests that advertised the same tools share it and two that differ anywhere
/// do not. Order participates: a model reads the list in the order it is given.
pub(crate) fn tool_catalog_version(tools: &[Tool]) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for tool in tools {
        tool.name.hash(&mut hasher);
        tool.description.hash(&mut hasher);
        tool.parameters.to_string().hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// The fingerprint of the window a request was assembled from.
///
/// Folded from the same per-entry digest the persistence lane computes to
/// coalesce snapshots, so an attempt's fingerprint and a snapshot's idea of
/// "unchanged" cannot disagree.
pub(crate) fn source_context_digest(window: &ContextWindow, stage_name: &str) -> String {
    let snapshot = crate::persistence::build_context_snapshot(window, stage_name);
    leviath_core::run_archive::digest_context(&snapshot).fingerprint()
}

/// The parameters a built request really carries, after every override and
/// clamp.
///
/// Read off the assembled request rather than off the stage's declaration,
/// because the two differ routinely: the completion budget is whatever the
/// window had room for, and a model that does not take a temperature gets zero
/// whatever the blueprint asked.
///
/// `max_output_tokens` is spelled as a stage spells it, so a reader parses one
/// vocabulary for a declared cap and an effective one.
pub(crate) fn effective_parameters(
    request: &InferenceRequest,
) -> std::collections::BTreeMap<String, serde_json::Value> {
    let mut table = std::collections::BTreeMap::new();
    table.insert(
        "temperature".to_string(),
        serde_json::Value::from(request.temperature),
    );
    table.insert(
        "max_output_tokens".to_string(),
        serde_json::Value::from(request.max_tokens),
    );
    request.request_timeout_secs.into_iter().for_each(|secs| {
        table.insert(
            "request_timeout_secs".to_string(),
            serde_json::Value::from(secs),
        );
    });
    // Whatever the stage passed through for the provider (`top_p`, `stop`,
    // `seed`, a retention knob), flattened in beside the two every provider
    // takes. `Null` when the stage set none, which is the ordinary case.
    request
        .extra
        .as_object()
        .into_iter()
        .flatten()
        .for_each(|(key, value)| {
            table.insert(key.clone(), value.clone());
        });
    table
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
    // What is left of the window for the answer, less the headroom the estimate
    // behind `remaining` needs. See [`PROMPT_ESTIMATE_HEADROOM`].
    let room_for_answer = remaining.saturating_sub(spent / PROMPT_ESTIMATE_HEADROOM);
    let max_tokens = room_for_answer.min(output_cap).max(MIN_OUTPUT_TOKENS);
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

    // A model that cannot call tools is refused by its provider for any
    // function call in the request, the history included. Everything a tool
    // did earlier in the run reaches it as prose instead, and nothing is
    // advertised to it.
    let (messages, filtered_tools) = if caps.supports_tools {
        (assembled.messages, filtered_tools)
    } else {
        // `submit_output` alone is what an output stage is always granted, and
        // a stage on an image or video model is exactly that: nothing the
        // blueprint asked for is being withheld, so there is nothing to warn of.
        if filtered_tools
            .iter()
            .any(|t| t.name != leviath_tools::SUBMIT_OUTPUT_TOOL)
        {
            tracing::warn!(
                model = %stage.model,
                tools = filtered_tools.len(),
                "the model cannot call tools; the stage's tools are not advertised to it"
            );
        }
        (
            leviath_providers::flatten_tool_turns(assembled.messages),
            Vec::new(),
        )
    };

    let mut system = hint_blocks(config, &filtered_tools, std::env::consts::OS);
    system.extend(assembled.system_blocks);

    // A model that does not read a system prompt has the stage's instruction
    // folded into the user turn instead, or it is lost. It lands in the system
    // blocks with a bare "Begin." user nudge (the convention that makes a text
    // model act); a model that ignores the system prompt generates from the
    // nudge - an image model's "Begin." becomes generic "start of a journey"
    // scenery, never the asked subject. The capability says whether the model
    // reads the system prompt; `ignores_system_prompt` is the one-off for a
    // model no catalogue distinguishes (`gemini-2.5-flash-image`).
    let mut messages = messages;
    let reads_system = caps.supports_system_prompt
        && !leviath_providers::capabilities::ignores_system_prompt(&stage.model);
    if !reads_system {
        fold_system_into_user(&mut system, &mut messages);
    }

    let request = InferenceRequest {
        system,
        messages,
        model: stage.model.clone(),
        max_tokens,
        temperature,
        tools: filtered_tools,
        extra,
        request_timeout_secs: config.and_then(|c| c.request_timeout_secs),
    };
    (request, system_hash, block_hashes)
}

/// Fold the system blocks into the first user turn and clear them, for a model
/// that does not read a system prompt. Done into the *first* user message, once,
/// so a multi-turn conversation keeps its shape: the bare "Begin." nudge is
/// replaced outright, a real text turn is prefixed, and a turn that carries
/// blocks (an input image) gains the text ahead of them. With no user turn at
/// all the folded system becomes one.
pub(crate) fn fold_system_into_user(
    system: &mut Vec<leviath_providers::SystemBlock>,
    messages: &mut Vec<leviath_providers::Message>,
) {
    let text = system
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.is_empty() {
        return;
    }
    system.clear();
    match messages.iter_mut().find(|m| m.role == "user") {
        Some(first) => match &mut first.content {
            leviath_providers::MessageContent::Text(existing) => {
                *existing = match existing.trim() == leviath_providers::OPENING_TURN {
                    true => text,
                    false => format!("{text}\n\n{existing}"),
                };
            }
            leviath_providers::MessageContent::Blocks(blocks) => {
                blocks.insert(0, leviath_providers::ContentBlock::Text { text });
            }
        },
        None => messages.push(leviath_providers::Message {
            role: "user".to_string(),
            content: leviath_providers::MessageContent::Text(text),
            cache_breakpoint: false,
            reasoning: None,
        }),
    }
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
pub(crate) struct InFlightWork(pub Vec<crate::cancel::CancelToken>);

/// Stop the in-flight work of every agent that has reached a terminal state, and
/// drop the handles. Runs before the dispatch systems each tick, so a cancel
/// takes effect on the very next tick rather than whenever the provider or tool
/// happens to answer.
pub(crate) fn abort_terminal_work(
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
    Option<&'static CaptureModelInput>,
);

/// The system prefix the last request sent, as a digest.
///
/// Kept per agent because that is the granularity Anthropic's prefix cache
/// works at: one run's blocks, in one order. Absent before the first request,
/// which is exactly when there is nothing to invalidate.
#[derive(bevy_ecs::component::Component, Debug, Clone, Copy)]
pub(crate) struct SystemPrefixHash(pub u64);

/// The previous request's per-block system digests, kept on the agent so the
/// next assembly can tell which blocks held still and place cache breakpoints
/// only where the entry is readable back.
#[derive(Component, Debug, Clone, Default)]
pub(crate) struct SystemBlockHashes(pub Vec<u64>);

/// The optional resources dispatch reads, as one parameter: the operator's
/// circuit and retry settings, and the mime store, registry and limits. Every
/// one is optional because a world assembled by hand in a test installs none
/// of them, and each has a built-in answer for that case.
#[derive(bevy_ecs::system::SystemParam)]
pub(crate) struct DispatchTuning<'w, 's> {
    /// Which providers' circuits are open.
    pub circuits: Option<Res<'w, ProviderCircuits>>,
    /// When a circuit opens and how long it stays open.
    pub policy: Option<Res<'w, CircuitPolicy>>,
    /// The retry schedule.
    pub retry: Option<Res<'w, InferenceRetryTuning>>,
    /// The journal lane, so each attempt the job makes is recorded where the
    /// rest of the run is. Absent in an in-memory world, which then records
    /// nothing rather than failing to dispatch.
    pub persist: Option<Res<'w, PersistenceStage>>,
    /// The mime store, registry and limits.
    pub mime: crate::blob_store::MimeParams<'w, 's>,
}

/// Inference-dispatch system: for every `ReadyToInfer` agent, resolve its
/// provider and, **if a per-model permit is free**, build the request, spawn the
/// inference job, and move it to `AwaitingInference`. If its provider is missing
/// or no slot is free, it stays `ReadyToInfer` and is retried on a later tick -
/// no blocking, no wasted task.
pub(crate) fn dispatch_inference(
    agents: Query<InferenceQuery, With<ReadyToInfer>>,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    tuning: DispatchTuning,
    par_commands: ParallelCommands,
) {
    let DispatchTuning {
        circuits,
        policy,
        retry,
        persist,
        mime,
    } = tuning;
    // Fan out across ready agents: request assembly (`build_request`) is the
    // per-agent CPU cost and is independent, so it runs in parallel on the
    // compute pool. Permit acquisition (an atomic semaphore) and the tokio spawn
    // are thread-safe; the marker swap is batched via `ParallelCommands`.
    //
    // This is the one system whose per-agent body runs off the driver thread, so
    // the thread-local `tick_scope` can't carry an entity back to the catcher.
    // Each agent's share runs under `run_agent_parallel`, which catches there -
    // where the entity is known - and marks that agent for `tick` to fail
    // Clearing the thread-local keeps a panic in the fan-out
    // machinery *itself* unattributed rather than blamed on whichever agent a
    // previous system left recorded.
    crate::tick_scope::clear();
    let now = chrono::Utc::now().timestamp();
    let circuit_policy = policy.map(|p| *p).unwrap_or_default();
    // The daemon inserts this from `[limits]`; a world that never set it (every
    // embedded host, and most tests) gets the built-in schedule.
    let retry_tuning = retry.map(|r| *r).unwrap_or_default();
    let circuits = circuits.as_deref();
    let persist = persist.as_deref();
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
            capture,
        )| {
            crate::tick_scope::run_agent_parallel(entity, &par_commands, &mut || {
                if state.status != AgentStatus::Active {
                    return; // paused / waiting / cancelled - don't start new work
                }
                // Every decline below records why and since when, so the
                // watchdog can tell a run that is waiting from one that is
                // waiting for something that will never happen.
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
                // three. The stall watchdog ends the wait.
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
                // Zero retention asked for: the providers that take it per
                // request get their field here, keyed by the name the stage
                // resolved to, which is the one the registry knows.
                let mut request = request;
                providers
                    .0
                    .apply_retention_knobs(&si.provider_name, &mut request.extra);
                // Remembered for the next request, which is the only way the
                // breakpoint decision can be made on evidence.
                par_commands.command_scope(|mut commands| {
                    commands
                        .entity(entity)
                        .insert(SystemPrefixHash(system_hash));
                    commands
                        .entity(entity)
                        .insert(SystemBlockHashes(block_hashes.clone()));
                    // What the window believes this call will cost, and what
                    // its stored parts will be billed over that. The response
                    // says what it really cost, and the three together are
                    // the only measurement of the estimator's drift the
                    // runtime gets.
                    commands
                        .entity(entity)
                        .insert(crate::pipeline::PromptEstimate(
                            window.current_tokens,
                            native_media_tokens(
                                &request,
                                &provider.mime(&si.model),
                                config.map(|c| c.as_text.as_slice()).unwrap_or_default(),
                            ),
                        ));
                });
                // A provider that does not advertise streaming for this model
                // is called non-streaming whatever the config says: `infer_stream`
                // has a default that buffers and then emits one chunk, so
                // asking anyway would pay for the fold and gain nothing.
                let stream =
                    stage.stream_inference && provider.capabilities(&si.model).supports_streaming;
                // The bytes of the request's stored parts are read in the
                // job, off this thread, against what this model takes, typed
                // by this run's registry. Every `PipelineWorld` installs the
                // store; a world assembled by hand in a test may not, and
                // then stored parts go out as their stand-ins.
                let (mime_resources, max_media_bytes) = mime.hydration_inputs(entity);
                let limits = provider.media_limits(&si.model);
                let settings = providers.0.retention_settings();
                let ttl_secs = mime.provider_file_ttl_secs();
                let hydration = mime_resources.map(|(store, registry)| {
                    let (files, why_inline) = crate::provider_files::route_for(
                        &provider,
                        &si.provider_name,
                        &limits,
                        settings,
                        store.as_ref(),
                        &state.agent_id,
                        ttl_secs,
                    );
                    crate::inference_bridge::JobHydration {
                        store,
                        run_id: state.agent_id.clone(),
                        registry,
                        mime: provider.mime(&si.model),
                        max_media_bytes: limits.inline_request_bytes.unwrap_or(max_media_bytes),
                        as_text: config.map(|c| c.as_text.clone()).unwrap_or_default(),
                        limits,
                        files,
                        why_inline,
                    }
                });
                // What every attempt at this call has in common, worked out
                // here because this is the last place the run, the stage, the
                // configured provider name and the assembled request exist
                // together: the job reports only an outcome, and by the time a
                // retry happens the stage has moved on.
                //
                // The digest is the request's identity rather than its content.
                // Two attempts that share one are the same request sent twice,
                // which is the question a retry raises; the request itself is
                // already in the window and has no business being copied into
                // the journal once per attempt.
                //
                // The exact request rides along only for a run whose operator
                // asked for it. The window fingerprint is computed here for the
                // same reason the digest is, and only under capture: folding it
                // walks every entry of the window, which is a cost no run that
                // is not being captured should pay.
                let journal = persist.map(|lane| crate::inference_bridge::AttemptJournal {
                    run_id: state.agent_id.clone(),
                    stage: state.current_stage.clone(),
                    provider: si.provider_name.clone(),
                    model: si.model.clone(),
                    lane: lane.0.clone(),
                    digest: leviath_core::run_archive::RequestDigest {
                        system_hash,
                        messages: request.messages.len(),
                        tools: request.tools.len(),
                        max_tokens: request.max_tokens,
                        temperature: request.temperature,
                    },
                    model_input: crate::inference_bridge::ModelInputPlan {
                        capture: capture.is_some(),
                        source_context_digest: capture
                            .map(|_| source_context_digest(window, &state.current_stage))
                            .unwrap_or_default(),
                        parameters: effective_parameters(&request),
                        tool_catalog_version: tool_catalog_version(&request.tools),
                    },
                });
                let job = InferenceJob {
                    entity,
                    // Checked here, against the registry's live settings,
                    // as well as at spawn: zero retention switched on under
                    // a running daemon holds from the next call.
                    refused: providers.0.retention_refusal(&si.provider_name, &si.model),
                    provider,
                    request,
                    permit,
                    calibration: calibration.copied(),
                    stream,
                    hydration,
                    journal,
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
                            attempt_id: String::new(),
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
