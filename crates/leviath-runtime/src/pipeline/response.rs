//! Response collection and stage-progress accounting.

use super::*;

/// The response has been applied and is ready to be examined for tool calls (or
/// completion) by the process-response system.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessResponse;

/// The receiving end of the inference-outcomes channel, as a world resource for
/// the collect system. (The sending end lives in [`InferenceStage`].)
#[derive(Resource)]
pub(crate) struct InferenceResults(pub UnboundedReceiver<InferenceOutcome>);

/// Convert a provider response into the stored `InferenceResult` component.
/// (Ported from `AgentEngine::apply_inference_response`.) `parts` are the
/// response's mime once stored, from [`store_model_parts`].
pub(crate) fn to_inference_result(
    response: &leviath_providers::InferenceResponse,
    parts: Vec<leviath_core::mime::Part>,
    attempt_id: &str,
) -> crate::components::InferenceResult {
    crate::components::InferenceResult {
        attempt_id: attempt_id.to_string(),
        response: response.content.clone(),
        parts,
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
        cut_off_at: (response.finish_reason == leviath_providers::FinishReason::TokenLimit)
            .then_some(response.tokens_used.completion_tokens),
        reasoning: response.reasoning.clone(),
    }
}

/// Put the mime a model produced into the run's store, each as a stored
/// part named as the provider named it. A blob the run cannot keep (no
/// store, over the ceiling) becomes a text part saying so, so the model's
/// own reply still records that it made something.
pub(crate) fn store_model_parts(
    blobs: Vec<leviath_core::mime::Blob>,
    entity: Entity,
    run_id: &str,
    mime: &crate::blob_store::MimeParams,
) -> Vec<leviath_core::mime::Part> {
    if blobs.is_empty() {
        return Vec::new();
    }
    // Drop byte-identical duplicates a model returned in one reply. Some
    // gateways echo the same file more than once - a streamed image resent on a
    // later delta, an `images` array with a repeat - and the store is
    // content-addressed, so two identical blobs are one file on disk anyway;
    // keeping both parts would only send the model its own output back twice.
    // Exact bytes only: a model that returns two genuinely different files,
    // even near-identical ones, keeps both.
    let blobs = dedupe_identical_blobs(blobs);
    let (sources, _) = mime.hydration_inputs(entity);
    let Some((store, registry)) = sources else {
        return blobs
            .into_iter()
            .map(|blob| {
                dropped_part(
                    run_id,
                    &format!(
                        "{} of {}, this run has no blob store",
                        blob.mime_type,
                        leviath_core::mime::human_size(blob.bytes.len() as u64)
                    ),
                )
            })
            .collect();
    };
    let sink = crate::context_setup::PartSink {
        store: store.as_ref(),
        registry: &registry,
        run_id,
        max_part_bytes: mime.max_part_bytes(),
        inline_text_bytes: mime.inline_text_bytes(),
    };
    blobs
        .into_iter()
        .enumerate()
        .map(|(i, blob)| {
            let name = blob
                .name
                .clone()
                .unwrap_or_else(|| format!("model-{}", i + 1));
            let mut inbound = leviath_core::mime::InboundPart::from_bytes(name, blob.bytes);
            inbound.mime_type = Some(blob.mime_type);
            sink.store_part(&inbound)
                .unwrap_or_else(|e| dropped_part(run_id, &e))
        })
        .collect()
}

/// The text every part the run could not keep begins with, so a stage log or
/// a test can pick the notes out of a reply's parts.
pub(crate) const DROPPED_PART_PREFIX: &str = "[model output dropped: ";

/// The text part that stands where a produced part should be, and the
/// warning that goes with it. Both, because a stage that then has "nothing
/// to hand back" must read as a ceiling in the run's log, not as a model
/// that made nothing.
fn dropped_part(run_id: &str, why: &str) -> leviath_core::mime::Part {
    tracing::warn!(run = %run_id, "[mime] produced part dropped: {why}");
    leviath_core::mime::Part::text(format!("{DROPPED_PART_PREFIX}{why}]"))
}

/// The notes [`dropped_part`] left among a reply's parts, for the stage log.
pub(crate) fn dropped_part_notes(parts: &[leviath_core::mime::Part]) -> Vec<String> {
    parts
        .iter()
        .filter_map(|p| p.inline_text())
        .filter(|t| t.starts_with(DROPPED_PART_PREFIX))
        .map(|t| format!("[mime] {}", t.trim_matches(['[', ']'])))
        .collect()
}

/// Keep the first of each byte-identical blob, in the order they arrived.
/// Identity is the sha256 of the bytes, the same key the blob store uses, so
/// this drops exactly what the store would have collapsed to one file.
fn dedupe_identical_blobs(blobs: Vec<leviath_core::mime::Blob>) -> Vec<leviath_core::mime::Blob> {
    let mut seen = std::collections::HashSet::new();
    blobs
        .into_iter()
        .filter(|b| seen.insert(leviath_core::mime::sha256_hex(&b.bytes)))
        .collect()
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
pub(crate) fn collect_inference(
    mut results: ResMut<InferenceResults>,
    mut agents: Query<InferenceQuery, With<AwaitingInference>>,
    mut circuits: Option<ResMut<ProviderCircuits>>,
    policy: Option<Res<CircuitPolicy>>,
    persist: Option<Res<crate::pipeline::persist::PersistenceStage>>,
    mime: crate::blob_store::MimeParams,
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
            match failed.and_then(|e| e.unavailable_reason().map(|r| (e, r))) {
                Some((err, reason)) => {
                    // The kind travels with the reason: a provider that
                    // accepted the connection and then answered slowly is not
                    // the same as one that refused it, and the breaker gives the
                    // first far more rope before taking it away from every run.
                    let kind = err.failure_kind();
                    let opened =
                        circuits.record_failure(&called_provider, reason, kind, now, &policy);
                    // The words, kept for the watchdog: a run that later finds
                    // every provider out of service says what the last one
                    // actually answered, not only that it stopped.
                    circuits.note_error(&called_provider, err.describe());
                    if opened {
                        // Loud and once, on the transition only: without it,
                        // ten dead runs in a row look like ten unrelated
                        // failures.
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
                // decides whether the run finishes.
                calibrate(
                    &mut commands,
                    outcome.entity,
                    calibration.as_deref_mut(),
                    estimate,
                    response.tokens_used.prompt_tokens,
                );
                let parts = store_model_parts(
                    response.parts.clone(),
                    outcome.entity,
                    &state.agent_id,
                    &mime,
                );
                // Buffer the readable output + a token line for the stage's
                // logs, and a line for each produced part the run could not
                // keep, so the log says why a stage has nothing to hand back.
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
                    for note in dropped_part_notes(&parts) {
                        buffer.logs.push((idx, note));
                    }
                }
                let result = to_inference_result(&response, parts, &outcome.attempt_id);
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
                // try again rather than killing the run.
                // Logged before the failover decision, and for every failure
                // rather than only the ones that fail over. A call that dies
                // without a fallback is exactly the one somebody has to
                // diagnose, and the error text on its own is the same sentence
                // for every transport failure, whether the hostname was wrong,
                // the port was closed, or the certificate had expired.
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
                    // Journaled from here rather than from the lane, because
                    // here is where the decision is made: the job reported a
                    // failure and knew nothing about a second candidate. Without
                    // this record the run's attempts change provider between one
                    // and the next with nothing saying who moved them, which
                    // reads as a run that was always configured this way.
                    //
                    // Keyed on the agent id, which is the run id, so it lands in
                    // the same journal as the attempts it sits between. A world
                    // with no lane writes nothing, exactly as the attempts do.
                    if let Some(persist) = persist.as_deref() {
                        let record = leviath_core::run_archive::FailoverRecord {
                            stage: state.current_stage.clone(),
                            iteration: state.iteration,
                            from_provider: called_provider.clone(),
                            from_model: called_model.clone(),
                            to_provider: next.provider.clone(),
                            to_model: next.model.clone(),
                            reason: err
                                .unavailable_reason()
                                .map(leviath_providers::UnavailableReason::label)
                                .expect("a failover only happens for an unusable provider")
                                .to_string(),
                            kind: crate::inference_bridge::failure_label(&err),
                            at: now,
                        };
                        let _ = persist.0.send(PersistMsg::Append {
                            run_id: state.agent_id.clone(),
                            record: Box::new(
                                leviath_core::run_archive::RunRecord::InferenceFailover(record),
                            ),
                            ack: None,
                        });
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
                if let Some((blocker, message)) = super::park::setup_park(&err, &called_provider) {
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
pub(crate) struct ReadyForTools;

/// The response had no tool calls; the agent is ready for the empty-response
/// handler to decide finish vs. a "use your tools" nudge.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReadyForTransition;

/// The agent's current stage is complete; the transition system will resolve the
/// next stage (or completion).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolveTransition;

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
    // The bytes this request sent are billed at their real cost and charged
    // to the window as stand-ins; that difference is this request's alone,
    // not the estimator's, so it comes off before the comparison.
    let reported = reported.saturating_sub(estimate.1);
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
pub(crate) struct StageProgress {
    /// Total tool calls the agent has made in this stage.
    pub total_tool_calls: usize,
    /// Consecutive text-only responses that were nudged toward tool use.
    pub text_only_nudges: usize,
    /// Replies the output cap cut off that were sent back with an explanation
    /// instead of being taken as the answer: a text reply with a nudge, a tool
    /// call with its refusal. One count for both, bounded by
    /// `MAX_CUT_OFF_NUDGES`, so a model that cannot fit its reply in the
    /// model's own maximum still ends the stage whichever shape the reply takes.
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
    /// Unix seconds at which the agent last parked on a person (a tool
    /// approval, an `ask_user_*` question, a checkpoint). Stamped and cleared
    /// by `reflect_interaction_status`, which moves `stage_started_at` forward
    /// by the time the wait took when the prompt resolves: a person's hour is
    /// not the agent's, and a `stuck_after_minutes` edge must not fire on it.
    pub waiting_since: Option<i64>,
    /// `write_file`/`edit_file` calls made in this stage, keyed by target path.
    /// Feeds the `stuck_after_same_file_edits` threshold.
    pub edits_by_path: std::collections::HashMap<String, usize>,
    /// A `stuck` edge has already fired in this stage. One-shot per stage entry:
    /// without it a stuck interrupt whose edge became unavailable would ping-pong
    /// between [`detect_stuck_stage`] and [`resolve_transition`]'s resume arm.
    pub stuck_fired: bool,
    /// Image parts this stage has produced. A stage that declares image output
    /// but has produced none is one whose image generation is failing; the
    /// counter tells that apart from a stage that has already drawn something
    /// and is now wrapping up in text.
    pub images_produced: usize,
    /// Text-only replies nudged back because the stage expected an image and
    /// had produced none. Bounded by `MAX_NO_IMAGE_NUDGES` so a model that
    /// keeps refusing does not loop forever.
    pub no_image_nudges: usize,
}

/// How a stage ended, when that governs the transition. Absent ⇒ the stage
/// completed normally. Read by [`resolve_transition`] to follow an
/// `error`/`max_iterations`/`stuck`-conditioned edge (e.g. → error_recovery)
/// when the stage errored, hit its iteration cap, or stopped making progress.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub(crate) enum StageOutcome {
    /// The stage errored (carries the error message for the terminal case).
    Errored(String),
    /// The stage hit its `max_iterations` cap.
    MaxIterations,
    /// A `stuck` edge tripped mid-stage; carries the human-readable reason.
    Stuck(String),
}

/// One [`StageRecord`](leviath_core::run_meta::StageRecord) per blueprint stage,
/// seeded at spawn (names + `Pending`) and reconciled by `dispatch_persistence`
/// (status + timestamps), with per-stage tokens accrued by `collect_inference`.
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
pub(crate) struct StageIoBuffer {
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
    // For a stage that ends on cut-off tool calls: the error status, and the
    // `[error]` line in the stage log that says why.
    Option<&'static mut crate::components::AgentState>,
    Option<&'static mut StageIoBuffer>,
    Option<&'static StageCursor>,
);

/// Process-response system: route each `ProcessResponse` agent by whether its
/// last inference asked for tools. Tool calls present ⇒ `ReadyForTools` (and the
/// stage's running tool-call count is bumped); none ⇒ `ReadyForTransition`. Pure
/// routing - no I/O.
pub(crate) fn process_response(
    mut agents: Query<ProcessResponseQuery, With<ProcessResponse>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, result, mut progress, totals, state, buffer, cursor) in agents.iter_mut() {
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
        // A call the cap cut off mid-argument arrives as text. Its refusal is
        // this path's nudge, so it spends the same budget a cut-off text
        // reply does. Both conditions are needed: text arguments alone can
        // also be a torn journal record, and a reply the cap stopped after
        // its calls were complete ran them as usual.
        let cut_off_call = result.cut_off_at.is_some()
            && result.tool_calls.iter().any(|c| c.arguments.is_string());
        let cut_off_text = result.cut_off_at.is_some() && result.tool_calls.is_empty();
        if cut_off_call {
            if progress.cut_off_nudges >= MAX_CUT_OFF_NUDGES {
                // Nothing in this reply can run, and the model has been told
                // how to split the call as many times as the budget allows. A
                // stage error, not a stage end: an `error` edge takes the run
                // to recovery with the reason, and without one the run fails
                // rather than reporting complete with the work undone.
                let tools: Vec<&str> = result.tool_calls.iter().map(|c| c.name.as_str()).collect();
                let message = cut_off_stage_error(progress.cut_off_nudges + 1, &tools);
                tracing::warn!(stage_error = %message, "ending the stage");
                if let Some(mut buffer) = buffer {
                    let idx = cursor.map_or(0, |c| c.index);
                    buffer.logs.push((idx, format!("[error] {message}")));
                }
                if let Some(mut state) = state {
                    state.status = AgentStatus::Error {
                        message: message.clone(),
                    };
                }
                e.insert(StageOutcome::Errored(message))
                    .insert(ResolveTransition);
                continue;
            }
            progress.cut_off_nudges += 1;
        } else if !cut_off_text {
            // Counted in a row: a reply that was not cut off (any number of
            // ordinary tool calls, or an answer) shows the model got past it,
            // so a later cut-off starts a fresh budget. A cut-off text reply
            // is counted by `handle_empty_response`.
            progress.cut_off_nudges = 0;
        }
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
pub(crate) struct GlobalNudge(pub leviath_core::NudgeConfig);

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
    Option<&'static crate::components::AgentState>,
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
pub(crate) fn handle_empty_response(
    mut agents: Query<EmptyResponseQuery, With<ReadyForTransition>>,
    mime: crate::blob_store::MimeParams,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, state, mut window, infer, mut progress, bp, cursor, global) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        let stage = bp.0.stages.get(cursor.index);
        // Where a long reply is stored, when the world has a store and this
        // run is known by id.
        let (sources, _) = mime.hydration_inputs(entity);
        let sink = state.and_then(|state| {
            crate::context_setup::PartSink::over(&sources, &state.agent_id, &mime)
        });
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
            store_reply(
                &mut window,
                infer,
                infer.reasoning.clone(),
                stage,
                sink.as_ref(),
            );
            inject_system_nudge(&mut window, &cut_off_nudge(cut_off_at));
            commands
                .entity(entity)
                .remove::<ReadyForTransition>()
                .insert(ReadyToInfer);
            continue;
        }
        // A stage that produces an image but just returned text, and has drawn
        // nothing so far, is one whose image generation is failing: a refusal, a
        // content filter, or an error string in place of a data URI. Count what
        // this turn drew first (an image reply has no tool calls, so it lands
        // here too); if the stage still has no image, send the model's own words
        // back so the retry is informed. Bounded, so a model that keeps refusing
        // lets the stage end rather than looping.
        if let Some(family) = stage_expected_media(stage) {
            progress.images_produced += media_part_count(&infer.parts, family);
            if progress.images_produced == 0 && progress.no_image_nudges < MAX_NO_IMAGE_NUDGES {
                progress.no_image_nudges += 1;
                tracing::warn!(
                    stage = stage.map(|s| s.name.as_str()).unwrap_or(""),
                    family,
                    "a media stage returned text and nothing it makes; likely a generation failure"
                );
                store_reply(
                    &mut window,
                    infer,
                    infer.reasoning.clone(),
                    stage,
                    sink.as_ref(),
                );
                inject_system_nudge(&mut window, &no_media_nudge(&infer.response, family));
                commands
                    .entity(entity)
                    .remove::<ReadyForTransition>()
                    .insert(ReadyToInfer);
                continue;
            }
        }
        // A reply that produced a part (a mesh from a 3D generator, an image
        // from a drawing model) has done the stage's work even with no text and
        // no tool call: its output is the part, not a call it forgot to make.
        // Accept it rather than nudging "use your tools" at a stage whose whole
        // answer is what it just produced. (The image-failure case above has
        // already had its say: it only nudges when nothing was drawn.)
        let produced_a_part = !infer.parts.is_empty();
        if progress.total_tool_calls > 0
            || produced_a_part
            || !nudge.enabled
            || progress.text_only_nudges >= nudge.max
        {
            // The reply is accepted as the stage's last word, so it goes into
            // the conversation like every other turn. Drop it here and a
            // transition gate that bounces the stage back is answered by a
            // model with no memory of what it had just said - and a stage
            // told "you have not written the file yet" with its own unwritten
            // draft in front of it can split it; one with nothing in front of
            // it drafts the whole thing again.
            store_reply(
                &mut window,
                infer,
                infer.reasoning.clone(),
                stage,
                sink.as_ref(),
            );
            commands
                .entity(entity)
                .remove::<ReadyForTransition>()
                .insert(ResolveTransition);
        } else {
            progress.text_only_nudges += 1;
            store_reply(
                &mut window,
                infer,
                infer.reasoning.clone(),
                stage,
                sink.as_ref(),
            );
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

/// How many text-only replies an image stage is nudged back before it is let
/// go. Small on purpose: an image model that returns text three times running
/// is refusing or erroring, not warming up, and the stage's own
/// `require_output`/`max_iterations` then ends it rather than looping.
pub(crate) const MAX_NO_IMAGE_NUDGES: usize = 3;

/// The media family a stage declares it makes (`image`, `video` or `audio`),
/// from a `format` or an `output_routing` target: `None` for a stage that
/// makes text. A plain prefix check, because both are opaque labels the
/// manifest already validated as mime patterns. Image first, then video, then
/// audio, when a stage names more than one.
pub(crate) fn stage_expected_media(
    stage: Option<&leviath_core::blueprint::Stage>,
) -> Option<&'static str> {
    let stage = stage?;
    let format = stage.output.as_ref().and_then(|o| o.format.as_deref());
    ["image", "video", "audio"].into_iter().find(|family| {
        let prefix = format!("{family}/");
        format.is_some_and(|f| f.starts_with(&prefix))
            || stage.output_routing.keys().any(|k| k.starts_with(&prefix))
    })
}

/// Parts of `family` among a reply's produced parts.
fn media_part_count(parts: &[leviath_core::mime::Part], family: &str) -> usize {
    let pattern = format!("{family}/*");
    parts
        .iter()
        .filter(|p| p.mime_type.matches(&pattern))
        .count()
}

/// The `[System]` line sent back when a media stage returned text and nothing
/// of the `family` it makes. It quotes the model's own words, because a
/// refusal or a filtered request states its reason there, so the retry is
/// informed rather than blind.
pub(crate) fn no_media_nudge(reply_text: &str, family: &str) -> String {
    let what = match family {
        "image" => "an image",
        "video" => "a video",
        _ => "audio",
    };
    let trimmed = reply_text.trim();
    if trimmed.is_empty() {
        return format!(
            "This stage produces {what}, but your last reply contained none. The \
             generation may have failed. Generate {what} and try again."
        );
    }
    let mut quoted = leviath_core::text::truncate_chars(trimmed, 500);
    if trimmed.chars().count() > 500 {
        quoted.push_str("...");
    }
    format!(
        "This stage produces {what}, but your last reply contained none, only text: \
         \"{quoted}\". That usually means the generation failed or was refused. If that \
         text names a problem, address it; then generate {what} and try again."
    )
}

/// Record a reply with no tool calls in the conversation as the model's
/// turn: its text and whatever mime it produced. A reply with nothing in it
/// (a cut-off tool call, an empty answer) leaves no entry: an empty
/// assistant message is noise to the next request and some providers refuse
/// it outright.
fn store_reply(
    window: &mut ContextWindow,
    infer: &crate::components::InferenceResult,
    reasoning: Option<String>,
    stage: Option<&leviath_core::blueprint::Stage>,
    sink: Option<&crate::context_setup::PartSink<'_>>,
) {
    // The stage may send some produced parts to regions of their own
    // (`output_routing`). The reply's text and any unrouted part stay in the
    // conversation as the assistant turn; the routed parts land in their
    // regions as separate entries.
    let routed = super::part_routing::split(stage, &infer.parts);
    if let Some(content) = reply_content(&infer.response, &routed.kept, sink) {
        let tokens = content.tokens(sink.map(|s| s.registry));
        let _ = window.add_turn(
            Some(leviath_core::ContextCause::ModelReply),
            "conversation",
            leviath_core::EntryKind::AssistantTurn { tool_calls: vec![] },
            content,
            tokens,
            reasoning,
        );
    }
    super::part_routing::store_routed(window, &routed);
}

/// A reply's text and produced parts as one entry's content, or `None` when
/// there is nothing to record. Text over `[mime] inline_text_bytes` is stored
/// through `sink` and the entry carries its stand-in; with no sink it stays
/// inline.
pub(crate) fn reply_content(
    text: &str,
    parts: &[leviath_core::mime::Part],
    sink: Option<&crate::context_setup::PartSink<'_>>,
) -> Option<leviath_core::region::EntryContent> {
    let mut all = Vec::with_capacity(parts.len() + 1);
    if !text.trim().is_empty() {
        all.push(crate::context_setup::text_part(sink, "reply.txt", text));
    }
    all.extend(parts.iter().cloned());
    (!all.is_empty()).then(|| leviath_core::region::EntryContent::from_parts(all))
}

/// Append a `[System]` nudge to the conversation region: the one injection path
/// shared by the empty-response nudge, the required-region nudges, and the
/// transition-gate hold, so every nudge reaches the model with the same shape.
/// (An unprefixed `Text` entry assembles as a user message, so the prefix is
/// what distinguishes framework guidance from real user input.)
pub(crate) fn inject_system_nudge(window: &mut ContextWindow, text: &str) {
    let content = format!("[System] {text}");
    let tokens = leviath_core::estimate_tokens(&content);
    let _ = window.add_to_region_caused(
        leviath_core::ContextCause::Framework,
        "conversation",
        content,
        tokens,
    );
}
