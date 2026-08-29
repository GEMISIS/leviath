//! Response collection and stage-progress accounting.

use super::*;

/// The response has been applied and is ready to be examined for tool calls (or
/// completion) by the process-response system.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessResponse;

/// The receiving end of the inference-outcomes channel, as a world resource for
/// the collect system. (The sending end lives in [`InferenceStage`].)
#[derive(Resource)]
pub struct InferenceResults(pub UnboundedReceiver<InferenceOutcome>);

/// Convert a provider response into the stored `InferenceResult` component.
/// (Ported from `AgentEngine::apply_inference_response`.)
pub(crate) fn to_inference_result(
    response: &leviath_providers::InferenceResponse,
) -> crate::components::InferenceResult {
    crate::components::InferenceResult {
        response: response.content.clone(),
        tool_calls: response
            .tool_calls
            .iter()
            .map(|tc| crate::components::ToolCall {
                tool_id: tc.id.clone(),
                name: tc.name.clone(),
                arguments: tc.arguments.clone(),
                thought_signature: tc.thought_signature.clone(),
            })
            .collect(),
        tokens_used: response.tokens_used.total_tokens,
        timestamp: chrono::Utc::now().timestamp(),
        cut_off_at: (response.finish_reason == leviath_providers::FinishReason::TokenLimit)
            .then_some(response.tokens_used.completion_tokens),
    }
}

/// What a person has to do about a provider that could not be reached.
///
/// A separate constant rather than a line-continued literal inside the
/// `format!`: rustfmt reflows those, and it silently baked the source's own
/// indentation into the middle of the sentence a user reads.
const UNREACHABLE_REMEDY: &str = "check the network connection, then `lev resume` this run";

/// Whether a failed provider call is the machine's problem rather than the
/// run's, and if so what to tell the person who has to fix it.
///
/// `None` means the run itself is what went wrong and the caller should fail it.
///
/// Two lanes ask - the stage call in [`collect_inference`] and the routing call
/// at a stage boundary in `collect_transition_choice` - and they used to answer
/// differently: the stage lane parked, the routing lane killed the run outright.
/// The same blip, a different outcome, decided by which call happened to be in
/// flight when the network went. The decision and the wording live here so the
/// two cannot drift again; what each lane must do to keep its own continuation
/// alive is still its own business, because those genuinely differ.
pub(super) fn setup_park(
    err: &leviath_providers::ProviderError,
    provider: &str,
) -> Option<(leviath_core::run_meta::SetupBlocker, String)> {
    use leviath_core::run_meta::SetupBlocker;
    use leviath_providers::UnavailableReason;

    match err.unavailable_reason()? {
        // Running out of credits is an account state, not a defect in the run:
        // the operator tops up and resumes. Failing here would make the run
        // permanently unresumable and throw away every iteration it has already
        // paid for, to punish somebody for a billing lapse. Unattended included
        // - a harness that cannot rescue a run cancels it instead (issue #456).
        UnavailableReason::CreditsExhausted => Some((
            SetupBlocker::CreditsExhausted,
            format!("out of credits ({err}): top up the account, then `lev resume` this run"),
        )),
        // The provider could not be reached and there is no candidate left to
        // try. That is the network being down, not the run being wrong: the
        // request never got an answer, so nothing about this run is known to be
        // bad, and the condition is usually over in seconds and always somebody
        // else's to fix.
        //
        // Reachable only once the retry policy is spent - a transport failure is
        // transient, so the dispatch job has already tried and backed off
        // `inference_retry_attempts` times before the outcome gets here.
        UnavailableReason::Unreachable => Some((
            SetupBlocker::ProvidersUnavailable,
            format!("could not reach '{provider}' ({err}): {UNREACHABLE_REMEDY}"),
        )),
        // A rejected key or a model the account may not have is a real setup
        // problem, but one the failover list may still route around, and the
        // stall watchdog already parks a run whose every candidate is out of
        // service (see `fail_stalled_dispatch`). Left to the caller's error
        // path so this change adds no new parking reason.
        UnavailableReason::AuthFailed | UnavailableReason::Forbidden => None,
    }
}

/// What `collect_inference` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type InferenceQuery = (
    &'static mut AgentState,
    Option<&'static crate::persistence::RunMetadata>,
    Option<&'static mut crate::persistence::TokenTotals>,
    Option<&'static StageCursor>,
    Option<&'static ContextWindow>,
    Option<&'static mut StageLedger>,
    Option<&'static mut StageIoBuffer>,
    Option<&'static mut StageInference>,
    Option<&'static mut crate::telemetry::StageActivity>,
    Option<&'static crate::pipeline::PromptEstimate>,
    Option<&'static mut crate::pipeline::PromptCalibration>,
);

/// Inference-collect system: drain completed inferences and apply them. A
/// success is stored on the agent (bumping its iteration) and the agent advances
/// to `ProcessResponse`; an error marks the agent `Error`. An outcome for an
/// agent that is no longer `AwaitingInference` (cancelled or despawned between
/// dispatch and now) is dropped.
pub fn collect_inference(
    mut results: ResMut<InferenceResults>,
    mut agents: Query<InferenceQuery, With<AwaitingInference>>,
    mut circuits: Option<ResMut<ProviderCircuits>>,
    policy: Option<Res<CircuitPolicy>>,
    persist: Option<Res<crate::pipeline::persist::PersistenceStage>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    let policy = policy.map(|p| *p).unwrap_or_default();
    let now = chrono::Utc::now().timestamp();
    while let Ok(outcome) = results.0.try_recv() {
        let Ok((
            mut state,
            md,
            mut totals,
            cursor,
            window,
            mut ledger,
            buffer,
            mut inference,
            activity,
            estimate,
            mut calibration,
        )) = agents.get_mut(outcome.entity)
        else {
            continue; // stale: agent cancelled/despawned since dispatch
        };
        crate::tick_scope::enter(outcome.entity);
        // The agent reached a terminal state while this inference was in flight
        // (a cancel, or a panic that failed it). Drop the response: applying it
        // would move the run on to `ProcessResponse` and it would keep going.
        if is_terminal_status(&state.status) {
            commands
                .entity(outcome.entity)
                .remove::<AwaitingInference>()
                .remove::<InFlightWork>();
            continue;
        }
        // The user paused the run while this inference was in flight. Pause is
        // documented as letting the in-flight step finish, so the outcome
        // arriving is expected - but it must not *act*. Applying a success walks
        // the run on through tool calls and stage changes while it still reads
        // `paused`; applying a failure overwrites the deliberate pause with
        // `Error` and throws away everything the run had done. Park the whole
        // outcome and let `resume` replay it through this same system.
        if state.status == AgentStatus::Paused {
            commands
                .entity(outcome.entity)
                .insert(crate::pipeline::HeldInference {
                    outcome,
                    lane: crate::pipeline::HeldLane::Stage,
                });
            continue;
        }
        let idx = cursor.map_or(0, |c| c.index);
        // Whoever we actually called. Read before the error arm below, which
        // may swap the component over to the next provider.
        let (called_provider, called_model) = inference
            .as_deref()
            .map(|i| (i.provider_name.clone(), i.model.clone()))
            .unwrap_or_default();
        // Record the call for the telemetry observer while the provider and
        // timing are still at hand (the observer only sees components).
        if let Some(mut activity) = activity {
            let usage = outcome.result.as_ref().ok().map(|r| &r.tokens_used);
            activity
                .0
                .push(crate::telemetry::ActivityRecord::Inference {
                    provider: called_provider.clone(),
                    model: called_model.clone(),
                    latency_ms: u64::try_from(outcome.latency.as_millis()).unwrap_or(u64::MAX),
                    prompt_tokens: usage.map_or(0, |u| u.prompt_tokens),
                    completion_tokens: usage.map_or(0, |u| u.completion_tokens),
                    cached_tokens: usage.map_or(0, |u| u.cached_tokens),
                    success: outcome.result.is_ok(),
                    // The same figure the run's own totals get, priced from the
                    // same rates a few lines below, so a dashboard and a run
                    // record cannot disagree about one call.
                    cost_usd: usage.and_then(|u| u.priced_cost(outcome.pricing.as_ref())),
                });
        }
        // Breaker bookkeeping, before the arms below consume the outcome. Any
        // answer at all proves the provider is serving; a provider-fatal one
        // counts against it and may take it out of service for everyone.
        if let Some(circuits) = circuits.as_deref_mut() {
            let failed = outcome.result.as_ref().err();
            match failed.and_then(|e| e.unavailable_reason()) {
                Some(reason) => {
                    // The kind travels with the reason now: a provider that
                    // accepted the connection and then answered slowly is not
                    // the same as one that refused it, and the breaker gives the
                    // first far more rope before taking it away from every run.
                    let kind = failed.and_then(|e| e.failure_kind());
                    if circuits.record_failure(&called_provider, reason, kind, now, &policy) {
                        // Loud and once, on the transition only. This is the
                        // alert issue #201 asked for: without it, ten dead
                        // runs in a row look like ten unrelated failures.
                        tracing::error!(
                            provider = %called_provider,
                            reason = reason.label(),
                            failure_kind = kind.map_or("unknown", |k| k.label()),
                            failures = policy.threshold_for(kind),
                            cooldown_secs = policy.cooldown_secs,
                            "provider circuit opened; no run will be dispatched to it \
                             until it recovers"
                        );
                    }
                }
                None if outcome.result.is_ok() => circuits.record_success(&called_provider),
                // An ordinary error says nothing about the provider either
                // way, so it neither counts against it nor clears its record.
                None => {}
            }
        }
        match outcome.result {
            Ok(response) => {
                state.iteration += 1;
                // This iteration's tokens and cost land on the run's totals, on
                // the current stage's ledger record and on its open visit, all
                // inside `record_call` - the one place that knows how a call is
                // priced.
                crate::inference_usage::record_call(
                    totals.as_deref_mut(),
                    ledger.as_deref_mut(),
                    persist.as_deref(),
                    md,
                    &crate::inference_usage::CallUsage {
                        kind: leviath_core::run_archive::InferenceKind::Stage,
                        stage: &state.current_stage,
                        iteration: state.iteration,
                        provider: &called_provider,
                        model: &called_model,
                        usage: &response.tokens_used,
                        pricing: outcome.pricing,
                    },
                );
                // What is left is per-stage bookkeeping that has nothing to do
                // with the invoice, and needs the window this call was built
                // from.
                //
                // Found by name, the same key `record_call` above just used and
                // the same one `restore_stage_ledger` matches on. Two lookups
                // for one call have to agree, and only one of them can be
                // written by index: the compaction lane has no cursor to offer.
                if let Some(rec) = ledger
                    .as_deref_mut()
                    .and_then(|l| l.0.iter_mut().find(|r| r.name == state.current_stage))
                {
                    // The high-water mark rather than a sum: a region is
                    // re-sent whole on every call, so summing would report a
                    // number that is neither what it costs per call nor what it
                    // holds. The largest it reached is the one that says
                    // whether it is earning its place.
                    //
                    // Every region the window carries, not only the ones this
                    // stage assembles: a stage layout hides the regions it does
                    // not declare rather than dropping them, and they are
                    // recorded here all the same.
                    for region in window.iter().flat_map(|w| w.regions.iter()) {
                        let seen = rec.region_tokens.entry(region.name.clone()).or_insert(0);
                        *seen = (*seen).max(region.current_tokens);
                    }
                    warn_if_context_is_running_away(rec, response.tokens_used.prompt_tokens);
                    // Persisted beside the runtime flag `process_response` sets,
                    // so a run resumed after a restart keeps the raised cap.
                    if response.finish_reason == leviath_providers::FinishReason::TokenLimit {
                        rec.output_cap_raised = true;
                    }
                }
                // The provider just said what this request really cost. Against
                // what the window believed it would cost, that is the only
                // measurement of the estimator's drift there is - and on a
                // provider whose window is a hard ceiling, drift is what
                // decides whether the run finishes (issue #485).
                calibrate(
                    &mut commands,
                    outcome.entity,
                    calibration.as_deref_mut(),
                    estimate,
                    response.tokens_used.prompt_tokens,
                );
                // Buffer the readable output + a token line for the stage's logs.
                if let Some(mut buffer) = buffer {
                    if !response.content.trim().is_empty() {
                        buffer.output.push((idx, response.content.clone()));
                    }
                    buffer.logs.push((
                        idx,
                        format!(
                            "[Tokens: {} in, {} out]",
                            response.tokens_used.prompt_tokens,
                            response.tokens_used.completion_tokens
                        ),
                    ));
                }
                let result = to_inference_result(&response);
                commands
                    .entity(outcome.entity)
                    .insert(result)
                    .remove::<AwaitingInference>()
                    .remove::<InFlightWork>()
                    .insert(ProcessResponse);
            }
            Err(err) => {
                // A request the pre-flight guard refused was measured with the
                // provider's own tokenizer, and that measurement is the only
                // evidence the window will get: the call never happened, so
                // there is no response to learn from. Folded in here so the
                // next request - the retry after compaction, or the resume -
                // is estimated from the figure that was just refused.
                if let leviath_providers::ProviderError::TokenLimitExceeded { used, .. } = &err {
                    calibrate(
                        &mut commands,
                        outcome.entity,
                        calibration.as_deref_mut(),
                        estimate,
                        *used,
                    );
                }
                // A provider that is out of credits or holding a rejected key
                // is not this request's problem: every later request to it
                // fails the same way. Move the stage to the next candidate and
                // try again rather than killing the run (issue #201).
                // Logged before the failover decision, and for every failure
                // rather than only the ones that fail over. A call that dies
                // without a fallback is exactly the one somebody has to
                // diagnose, and it used to leave nothing but the error text -
                // which for a transport failure is the same sentence whether
                // the hostname was wrong, the port was closed, or the
                // certificate had expired.
                tracing::warn!(
                    provider = %called_provider,
                    model = %called_model,
                    failure_kind = err
                        .failure_kind()
                        .map(leviath_providers::FailureKind::label)
                        .unwrap_or("unclassified"),
                    unavailable_reason = err
                        .unavailable_reason()
                        .map(leviath_providers::UnavailableReason::label)
                        .unwrap_or("none"),
                    error = %err,
                    "provider call failed"
                );
                let next = err.unavailable_reason().and_then(|_| {
                    let si = inference.as_deref_mut()?;
                    (!si.fallbacks.is_empty()).then(|| si.fallbacks.remove(0))
                });
                if let Some(next) = next {
                    // Loud on purpose. Silently swapping providers is how a
                    // factory ends up running on a model nobody chose.
                    tracing::warn!(
                        from_provider = %called_provider,
                        from_model = %called_model,
                        to_provider = %next.provider,
                        to_model = %next.model,
                        error = %err,
                        "provider unusable; failing over to the next configured model"
                    );
                    if let Some(mut buffer) = buffer {
                        buffer.logs.push((
                            idx,
                            format!(
                                "[failover] {called_provider}/{called_model} is unusable \
                                 ({err}); retrying on {}/{}",
                                next.provider, next.model
                            ),
                        ));
                    }
                    let si = inference
                        .as_deref_mut()
                        .expect("the failover branch only runs with a StageInference");
                    si.provider_name = next.provider;
                    si.model = next.model;
                    // Back to ready, not errored: the next tick dispatches it
                    // against the new provider and takes that model's permit.
                    // The iteration is deliberately not bumped - the agent has
                    // still not had a turn.
                    commands
                        .entity(outcome.entity)
                        .remove::<AwaitingInference>()
                        .remove::<InFlightWork>()
                        .insert(ReadyToInfer);
                    continue;
                }
                if let Some((blocker, message)) = setup_park(&err, &called_provider) {
                    tracing::warn!(
                        provider = %called_provider,
                        blocker = %blocker,
                        error = %err,
                        "pausing the run until the machine is fixed"
                    );
                    if let Some(mut buffer) = buffer {
                        buffer.logs.push((idx, format!("[paused] {message}")));
                    }
                    state.status = AgentStatus::Paused;
                    commands
                        .entity(outcome.entity)
                        .remove::<AwaitingInference>()
                        .remove::<InFlightWork>()
                        .insert(crate::pipeline::PausedForSetup {
                            blocker,
                            remedy: message,
                        })
                        // Kept on purpose: the retry is already staged, so a
                        // resume re-dispatches this same inference rather than
                        // rebuilding anything.
                        .insert(ReadyToInfer);
                    continue;
                }
                if let Some(mut buffer) = buffer {
                    buffer.logs.push((idx, format!("[error] {err}")));
                }
                // Record the error and route it to the stage's transition logic
                // (which follows an `error`-conditioned edge if the stage has one,
                // e.g. → error_recovery, or terminates the run otherwise).
                state.status = AgentStatus::Error {
                    message: err.to_string(),
                };
                commands
                    .entity(outcome.entity)
                    .remove::<AwaitingInference>()
                    .remove::<InFlightWork>()
                    .insert(StageOutcome::Errored(err.to_string()))
                    .insert(ResolveTransition);
            }
        }
    }
}

/// The response had tool calls; the agent is ready for the tool-dispatch system
/// to run them (the calls live on its `InferenceResult`).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyForTools;

/// The response had no tool calls; the agent is ready for the empty-response
/// handler to decide finish vs. a "use your tools" nudge.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyForTransition;

/// The agent's current stage is complete; the transition system will resolve the
/// next stage (or completion).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveTransition;

/// How much bigger than its first call a stage's prompt may get before the run
/// says so.
///
/// The runtime notices a stalled run and a stuck one; it noticed nothing about
/// the failure that actually costs money - a region filling up and being
/// re-sent on every call. Measured, a profile stage capped at 10 iterations
/// billed 1,135,289 tokens, roughly 113k per call, because an uncapped read had
/// filled its region. Nothing warned, and the run looked healthy from the
/// outside until the bill arrived.
///
/// Four rather than two: a stage that reads a file and then works with it has
/// genuinely grown, and warning about that would be noise. Four is past the
/// point where growth is explained by ordinary accumulation.
const RUNAWAY_CONTEXT_FACTOR: usize = 4;

/// Say so when a stage's per-call prompt has grown past
/// [`RUNAWAY_CONTEXT_FACTOR`] times its first call.
///
/// Once per stage, on the crossing. Repeating it every call afterwards would
/// bury the run's other output in exactly the situation where that output
/// matters.
pub(crate) fn warn_if_context_is_running_away(
    rec: &mut leviath_core::run_meta::StageRecord,
    prompt_tokens: usize,
) {
    let first = match rec.first_call_prompt_tokens {
        Some(first) => first,
        None => {
            rec.first_call_prompt_tokens = Some(prompt_tokens);
            return;
        }
    };
    if rec.runaway_warned || first == 0 || prompt_tokens < first * RUNAWAY_CONTEXT_FACTOR {
        return;
    }
    rec.runaway_warned = true;
    tracing::warn!(
        stage = %rec.name,
        first_call_prompt_tokens = first,
        this_call_prompt_tokens = prompt_tokens,
        "this stage's context has grown past {RUNAWAY_CONTEXT_FACTOR}x its first call and is \
         re-sent on every call; check whether a region is accumulating without a cap \
         (`lev stages <run-id>` shows the per-region sizes)"
    );
}

/// Fold one call's real cost into the agent's estimator correction, creating
/// the correction if this is its first measured call.
///
/// Split out because the insert-if-absent has to reach `Commands` while the
/// update does not, and the collect loop is long enough already. An agent with
/// no [`PromptEstimate`] never dispatched through the inference lane - a
/// compaction reply, or a test driving the outcome channel directly - and there
/// is nothing to compare, so it is left alone.
pub(crate) fn calibrate(
    commands: &mut Commands,
    entity: Entity,
    calibration: Option<&mut crate::pipeline::PromptCalibration>,
    estimate: Option<&crate::pipeline::PromptEstimate>,
    reported: usize,
) {
    let Some(estimate) = estimate else {
        return;
    };
    let (moved, shortfall) = match calibration {
        Some(calibration) => (
            calibration.observe(estimate.0, reported),
            calibration.shortfall(),
        ),
        None => {
            let mut fresh = crate::pipeline::PromptCalibration::default();
            let moved = fresh.observe(estimate.0, reported);
            let shortfall = fresh.shortfall();
            commands.entity(entity).insert(fresh);
            (moved, shortfall)
        }
    };
    // Said on the crossing only, so a steady run stays quiet. Without this the
    // correction is invisible: it changes when eviction fires, and an operator
    // watching a run get tighter with its context has no other way to see why.
    if moved {
        tracing::debug!(
            estimated = estimate.0,
            reported,
            shortfall,
            "the provider charged more than the context window accounted for; \
             budgeting against the measured figure from here"
        );
    }
}

/// Per-stage progress counters, reset when an agent enters a stage.
#[derive(Component, Debug, Clone, Default)]
pub struct StageProgress {
    /// Total tool calls the agent has made in this stage.
    pub total_tool_calls: usize,
    /// Consecutive text-only responses that were nudged toward tool use.
    pub text_only_nudges: usize,
    /// Replies the output cap cut off that were sent back with an explanation
    /// instead of being taken as the answer. Bounded by
    /// `MAX_CUT_OFF_NUDGES` so a model that cannot fit its reply in the
    /// model's own maximum still ends the stage.
    pub cut_off_nudges: usize,
    /// Set once a reply in this stage was cut off: the next requests go out
    /// with the output cap raised to the model's maximum, since the stage's
    /// own cap is what the reply did not fit under. Reset with the stage.
    pub raise_output_cap: bool,
    /// Inferences run in this stage (per-stage, unlike the run-cumulative
    /// `AgentState.iteration`), for enforcing the stage's `max_iterations`.
    pub iterations: usize,
    /// Successful file-modifying tool calls (`write_file`/`edit_file`, plus any
    /// tool named by an outgoing gate) made in this stage. Read by the
    /// transition gate to enforce `require_modifications`.
    pub modifying_tool_calls: usize,
    /// Modifying tool calls the permission layer refused (`[denied] ...`). A
    /// gate lets the transition through when this is non-zero: the agent is
    /// trying to write and cannot, so re-running the stage only burns budget.
    pub blocked_modification_calls: usize,
    /// Content digests of the regions this stage's outgoing gates watch, as
    /// they stood when the stage was entered.
    ///
    /// Only the watched regions: hashing every region on every entry would
    /// cost the whole window for a feature most stages do not use. Empty for a
    /// stage with no `require_region_updated` gate, which is the common case.
    pub entry_region_digests: std::collections::HashMap<String, u64>,
    /// How many times a transition gate has already sent this stage back for
    /// another pass. Bounded by the gate's `max_attempts`.
    pub gate_reentries: usize,
    /// Unix seconds of the first tick this agent was ready to infer in the
    /// stage - the clock a `stuck_after_minutes` threshold reads. Stamped
    /// lazily by [`detect_stuck_stage`] so spawn, `enter_stage` and
    /// [`force_transition`] all get a fresh clock from the `Default` reset
    /// without threading a clock through their signatures.
    pub stage_started_at: Option<i64>,
    /// `write_file`/`edit_file` calls made in this stage, keyed by target path.
    /// Feeds the `stuck_after_same_file_edits` threshold.
    pub edits_by_path: std::collections::HashMap<String, usize>,
    /// A `stuck` edge has already fired in this stage. One-shot per stage entry:
    /// without it a stuck interrupt whose edge became unavailable would ping-pong
    /// between [`detect_stuck_stage`] and [`resolve_transition`]'s resume arm.
    pub stuck_fired: bool,
}

/// How a stage ended, when that governs the transition. Absent ⇒ the stage
/// completed normally. Read by [`resolve_transition`] to follow an
/// `error`/`max_iterations`/`stuck`-conditioned edge (e.g. → error_recovery)
/// when the stage errored, hit its iteration cap, or stopped making progress.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub enum StageOutcome {
    /// The stage errored (carries the error message for the terminal case).
    Errored(String),
    /// The stage hit its `max_iterations` cap.
    MaxIterations,
    /// A `stuck` edge tripped mid-stage; carries the human-readable reason.
    Stuck(String),
}

/// One [`StageRecord`](leviath_core::run_meta::StageRecord) per blueprint stage,
/// seeded at spawn (names + `Pending`) and reconciled by [`dispatch_persistence`]
/// (status + timestamps), with per-stage tokens accrued by [`collect_inference`].
/// Serialized to `stages.json` so the dashboard / serve API can show every
/// stage's real name and status - not just the active one (whose name is the only
/// one carried in `meta.json`).
#[derive(Component, Debug, Clone)]
pub struct StageLedger(pub Vec<leviath_core::run_meta::StageRecord>);

/// Buffered per-stage output/log lines awaiting the persistence lane. Emitters
/// ([`collect_inference`], [`collect_tools`]) push; [`dispatch_persistence`]
/// drains and clears, forwarding the lines to `stages/<idx>/output.log` (readable
/// assistant output) and `stages/<idx>/logs.log` (tool + token + error events).
#[derive(Component, Debug, Clone, Default)]
pub struct StageIoBuffer {
    /// Readable assistant output lines, each tagged with its stage index.
    pub output: Vec<(usize, String)>,
    /// Operational log lines (tool activity, token counts, errors), each tagged
    /// with its stage index.
    pub logs: Vec<(usize, String)>,
}

/// What `process_response` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type ProcessResponseQuery = (
    Entity,
    &'static crate::components::InferenceResult,
    &'static mut StageProgress,
    Option<&'static mut crate::persistence::TokenTotals>,
);

/// Process-response system: route each `ProcessResponse` agent by whether its
/// last inference asked for tools. Tool calls present ⇒ `ReadyForTools` (and the
/// stage's running tool-call count is bumped); none ⇒ `ReadyForTransition`. Pure
/// routing - no I/O.
pub fn process_response(
    mut agents: Query<ProcessResponseQuery, With<ProcessResponse>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, result, mut progress, totals) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        progress.iterations += 1; // per-stage inference count (for max_iterations)
        // Whatever the reply held, the cap it did not fit under is not worth
        // sending again: a cut-off tool call is refused with the reason by
        // `dispatch_tools`, a cut-off text by `handle_empty_response`, and
        // both retries need the room the model actually has.
        if result.cut_off_at.is_some() {
            progress.raise_output_cap = true;
        }
        let mut e = commands.entity(entity);
        e.remove::<ProcessResponse>();
        if result.tool_calls.is_empty() {
            e.insert(ReadyForTransition);
        } else {
            progress.total_tool_calls += result.tool_calls.len();
            // Per-path edit churn, for `stuck` edges armed on same-file edits.
            // Counted from the *requested* calls: a model asking to edit the
            // same wrong file five times is stuck whether or not each call ran.
            for path in result.tool_calls.iter().filter_map(edited_path) {
                *progress.edits_by_path.entry(path.to_string()).or_insert(0) += 1;
            }
            if let Some(mut totals) = totals {
                totals.tool_calls += result.tool_calls.len();
            }
            e.insert(ReadyForTools);
        }
    }
}

/// The path a tool call targets, for per-stage edit-churn tracking. Only the two
/// mutating file tools count: both carry the path in their `path` argument. A
/// call without a string `path` (or any other tool) contributes nothing.
pub(crate) fn edited_path(call: &crate::components::ToolCall) -> Option<&str> {
    matches!(call.name.as_str(), "write_file" | "edit_file")
        .then(|| call.arguments.get("path").and_then(|v| v.as_str()))
        .flatten()
}

/// The global config's `[nudge]` defaults, captured per agent at spawn time so
/// a hot-reloaded config applies from the next run rather than mutating live
/// ones (same snapshot semantics as the batch-tool-hint global). Absent on
/// worlds that spawn agents without going through the seeded spawn (tests,
/// embedders); [`leviath_core::resolve_nudge`] then falls through to the
/// built-in defaults.
#[derive(Component, Debug, Clone, Default)]
pub struct GlobalNudge(pub leviath_core::NudgeConfig);

/// Whether this stage's deliverable *is* its text response.
///
/// A stage with interaction points presents what it writes for the user to
/// approve, revise or edit - the text is the work product, not a model stalling
/// before it starts. Nudging one is worse than wasteful: the nudge says "use
/// your tools to complete the task", and a stage built to produce a document
/// usually has no tool that could. A planning stage told to complete the task
/// went looking for a way to write the file, found none, and asked the user to
/// grant it a write tool or create the file by hand - instead of ending the
/// stage and presenting the plan it had already finished writing.
pub(crate) fn stage_output_is_reviewed(bp: &AgentBlueprint, cursor: &StageCursor) -> bool {
    matches!(
        bp.0.stages.get(cursor.index).map(|s| &s.mode),
        Some(leviath_core::blueprint::StageMode::InteractivePoints { points }) if !points.is_empty()
    )
}

/// What `handle_empty_response` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type EmptyResponseQuery = (
    Entity,
    &'static mut ContextWindow,
    &'static crate::components::InferenceResult,
    &'static mut StageProgress,
    &'static AgentBlueprint,
    &'static StageCursor,
    Option<&'static GlobalNudge>,
);

/// Empty-response system: for each `ReadyForTransition` agent decide whether the
/// stage is done. If the agent has already made tool calls, its nudge is
/// disabled, or it has been nudged its budgeted number of times, the text
/// response is accepted and the agent advances to `ResolveTransition`.
/// Otherwise (text only, no work yet) the response + the stage's nudge are
/// added to context and the agent loops back to `ReadyToInfer`. Ported from
/// `AgentEngine::loop_handle_empty_tool_calls`.
///
/// The nudge is programmable per stage (`[stages.<name>.nudge]`), per agent
/// (`[agent.nudge]`), and globally (config `[nudge]`), each field cascading
/// independently through [`leviath_core::resolve_nudge`]. With nothing
/// configured, a stage whose output is reviewed is never nudged - see
/// `stage_output_is_reviewed` - but an explicit `enabled` at any level speaks
/// for itself. The text supports `{stage}` and `{regions}` placeholders.
pub fn handle_empty_response(
    mut agents: Query<EmptyResponseQuery, With<ReadyForTransition>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, mut window, infer, mut progress, bp, cursor, global) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        let stage = bp.0.stages.get(cursor.index);
        let nudge = leviath_core::resolve_nudge(
            global.map(|g| &g.0),
            bp.0.nudge.as_ref(),
            stage.and_then(|s| s.nudge.as_ref()),
            stage_output_is_reviewed(bp, cursor),
        );
        // A reply the output cap cut off is not the stage's answer, however
        // many tool calls came before it. Keep what arrived so the model can
        // see it, say what happened, and go again with the cap raised (see
        // `StageProgress::raise_output_cap`). Bounded separately from the
        // text-only nudge: that one is off for a reviewed stage, and a
        // reviewed stage's document is exactly the reply most likely to be
        // cut off.
        if let Some(cut_off_at) = infer.cut_off_at
            && progress.cut_off_nudges < MAX_CUT_OFF_NUDGES
        {
            progress.cut_off_nudges += 1;
            store_text_reply(&mut window, &infer.response);
            inject_system_nudge(&mut window, &cut_off_nudge(cut_off_at));
            commands
                .entity(entity)
                .remove::<ReadyForTransition>()
                .insert(ReadyToInfer);
            continue;
        }
        if progress.total_tool_calls > 0 || !nudge.enabled || progress.text_only_nudges >= nudge.max
        {
            // The reply is accepted as the stage's last word, so it goes into
            // the conversation like every other turn. It used to be dropped
            // here, which meant a transition gate that bounced the stage back
            // was answered by a model with no memory of what it had just
            // said - and a stage told "you have not written the file yet"
            // with its own unwritten draft in front of it can split it; one
            // with nothing in front of it drafts the whole thing again.
            store_text_reply(&mut window, &infer.response);
            commands
                .entity(entity)
                .remove::<ReadyForTransition>()
                .insert(ResolveTransition);
        } else {
            progress.text_only_nudges += 1;
            store_text_reply(&mut window, &infer.response);
            let stage_name = stage.map(|s| s.name.as_str()).unwrap_or("");
            let regions = stage
                .and_then(|s| s.context_layout.as_ref())
                .unwrap_or(&bp.0.context_layout)
                .regions
                .iter()
                .filter(|r| r.required)
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let text = leviath_core::text::interpolate(
                &nudge.text,
                &[("stage", stage_name), ("regions", &regions)],
            );
            inject_system_nudge(&mut window, &text);
            commands
                .entity(entity)
                .remove::<ReadyForTransition>()
                .insert(ReadyToInfer);
        }
    }
}

/// How many times a stage sends a cut-off reply back before accepting what
/// it has. The first retry goes out with the cap raised to the model's
/// maximum, so a second cut-off means the reply does not fit the model at all
/// and the nudge asks for it in pieces; a third means the model is not
/// listening, and the stage ends rather than paying for a fourth.
pub(crate) const MAX_CUT_OFF_NUDGES: usize = 3;

/// The `[System]` line sent back with a cut-off reply.
///
/// It names the cause and the two ways out, because the reply that got cut
/// off was almost always a single oversized write, and a model told only "you
/// have not written the file yet" sends the same write again.
pub(crate) fn cut_off_nudge(cut_off_at: usize) -> String {
    format!(
        "Your previous reply was cut off by the output limit after {cut_off_at} output tokens, \
         so it was not used. Do not send it again as it was. Either make it shorter, or split \
         the work into smaller pieces: for a file, write the first part, then add each further \
         part with a separate call. The output limit has been raised to the model's maximum \
         for your next reply."
    )
}

/// Record a text-only reply in the conversation as the model's turn. A reply
/// with nothing in it (a cut-off tool call, an empty answer) leaves no entry:
/// an empty assistant message is noise to the next request and some
/// providers refuse it outright.
fn store_text_reply(window: &mut ContextWindow, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let tokens = leviath_core::estimate_tokens(text);
    let _ = window.add_typed_entry(
        "conversation",
        leviath_core::EntryKind::AssistantTurn { tool_calls: vec![] },
        text.to_string(),
        tokens,
    );
}

/// Append a `[System]` nudge to the conversation region: the one injection path
/// shared by the empty-response nudge, the required-region nudges, and the
/// transition-gate hold, so every nudge reaches the model with the same shape.
/// (An unprefixed `Text` entry assembles as a user message, so the prefix is
/// what distinguishes framework guidance from real user input.)
pub(crate) fn inject_system_nudge(window: &mut ContextWindow, text: &str) {
    let content = format!("[System] {text}");
    let tokens = leviath_core::estimate_tokens(&content);
    let _ = window.add_to_region("conversation", content, tokens);
}
