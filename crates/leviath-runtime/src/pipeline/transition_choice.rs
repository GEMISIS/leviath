//! The LLM-chosen transition: build the prompt that asks which stage runs
//! next, dispatch it as an inference, and match the answer back to one of the
//! edges that were offered.

use super::*;

/// A transition-choice inference is in flight (an LLM is picking the next stage);
/// holds the choosable edges so the collect system can match the response back to
/// one. (Ported from the async portion of `graph::prompt_llm_transition`.)
#[derive(Component, Debug, Clone)]
pub(crate) struct AwaitingTransitionResponse(pub Vec<leviath_core::blueprint::TransitionEdge>);

/// The receiving end of the transition-choice outcomes channel, as a world
/// resource for the collect system. (The sending end lives in
/// [`InferenceStage::transition_outcomes`].)
#[derive(Resource)]
pub(crate) struct TransitionResults(pub UnboundedReceiver<InferenceOutcome>);

/// Build the LLM prompt that asks which stage to run next. (Ported from the
/// prompt-building portion of `graph::prompt_llm_transition`.)
pub(crate) fn build_transition_prompt(
    stage: &leviath_core::Stage,
    edges: &[leviath_core::blueprint::TransitionEdge],
) -> String {
    let mut p = match &stage.transition_prompt {
        Some(custom) => {
            let mut p = custom.clone();
            p.push_str("\n\nAvailable transitions:\n");
            p
        }
        None => format!(
            "Stage '{}' is complete. Available next stages:\n",
            stage.name
        ),
    };
    for edge in edges {
        p.push_str(&format!("- {}", edge.target));
        if let Some(hint) = &edge.hint {
            p.push_str(&format!(": {hint}"));
        }
        p.push('\n');
    }
    if stage.transition_prompt.is_some() {
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
    } else if stage.allow_complete {
        p.push_str(
            "\nWhich stage should run next? Respond with ONLY the stage name, or ONLY the \
             word DONE if no further stage is needed and the run should end here.",
        );
    } else {
        p.push_str("\nWhich stage should run next? Respond with ONLY the stage name.");
    }
    p
}

/// Match an LLM transition response to one of the choosable edges' target stages,
/// or `None` if the stage may complete and the LLM chose to end here.
///
/// Models are asked to answer with only the target stage name (or `DONE`), but
/// frequently wrap it in prose or re-explain the stage. We therefore look for a
/// clean, standalone decision - scanning the first line, then the concluding
/// line, for a **whole-word** match against a stage name or `DONE` - instead of
/// substring-scanning the whole response, where a stage name mentioned in
/// passing ("the implementation", "the approved plan") would hijack the routing.
/// When nothing matches, a stage that may complete ends the run; otherwise the
/// run advances along the first declared edge.
pub(crate) fn match_transition_choice(
    choice: &str,
    edges: &[leviath_core::blueprint::TransitionEdge],
    allow_complete: bool,
) -> Option<String> {
    let lines: Vec<&str> = choice
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    // Candidate decision lines, in priority order: the first line (the model was
    // told to reply with only the name, so the answer leads), then - only if it
    // is short and answer-like (≤ 3 words) - the concluding line, which catches
    // models that reason first and answer last without matching a stage name
    // buried in a prose summary ("the approved plan was implemented").
    let words_in = |line: &str| {
        line.split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|w| !w.is_empty())
            .count()
    };
    let first = lines.first().copied();
    let last = lines
        .last()
        .copied()
        .filter(|l| lines.len() > 1 && words_in(l) <= 3);
    for line in first.into_iter().chain(last) {
        for word in line.split(|c: char| !c.is_alphanumeric() && c != '_') {
            if word.is_empty() {
                continue;
            }
            if allow_complete && word.eq_ignore_ascii_case("done") {
                return None;
            }
            if let Some(edge) = edges.iter().find(|e| word.eq_ignore_ascii_case(&e.target)) {
                return Some(edge.target.clone());
            }
        }
    }
    // No clear decision: a stage that may end prefers ending over looping back;
    // otherwise the run advances along the first declared edge.
    if allow_complete {
        None
    } else {
        edges.first().map(|edge| edge.target.clone())
    }
}

/// What `dispatch_transition_choice` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type TransitionChoiceQuery = (
    Entity,
    &'static AgentState,
    &'static mut ContextWindow,
    &'static StageInference,
    &'static AgentBlueprint,
    &'static StageCursor,
    &'static AwaitingTransitionChoice,
    Option<&'static InFlightWork>,
    Option<&'static DispatchStall>,
    // What the stage's own request was built from, so the routing request
    // can be built the same way and share its cached prefix.
    (
        Option<&'static crate::components::InferenceConfig>,
        Option<&'static crate::pipeline::response::StageProgress>,
        Option<&'static crate::pipeline::inference::SystemPrefixHash>,
        Option<&'static crate::pipeline::inference::SystemBlockHashes>,
        Option<&'static crate::pipeline::PromptCalibration>,
    ),
);

/// The shape of `tool_choice: none` for a provider's wire format, or `None`
/// for a provider whose format is not known here.
///
/// The routing request carries the stage's tool list so that its prefix is
/// the stage request's prefix - every provider caches the tool definitions
/// as part of it - and this is what stops the model from answering the
/// "which stage next?" question with a tool call instead of a name. A
/// provider this cannot vouch for gets a request with no tools at all,
/// which is a cold prefix but a safe one.
fn routing_tool_choice(provider: &str) -> Option<serde_json::Value> {
    match provider {
        "anthropic" => Some(serde_json::json!({ "type": "none" })),
        // Codex speaks the Responses API, which takes the same bare string.
        // Without an arm here the fallback clears the tool array instead,
        // which cold-reads the whole window on every stage transition.
        "openai" | "openrouter" | "gemini" | "codex" => Some(serde_json::json!("none")),
        _ => None,
    }
}

/// The routing request: the stage's own request, built by the same code, with
/// a short answer budget, a fixed temperature, and tool use switched off.
///
/// Building it through the stage's own builder is what keeps the two
/// prompts byte-identical up to the last block. Assemble it separately and
/// they diverge at the first byte, so every routing call is a cold read of
/// the whole context: on a 170,000-token window, a full re-send to get one
/// word back.
pub(crate) fn routing_request(
    window: &ContextWindow,
    config: Option<&crate::components::InferenceConfig>,
    si: &StageInference,
    provider: &Arc<dyn Provider>,
    stage_name: &str,
    stage_iterations: usize,
    prior: crate::pipeline::inference::PriorCalls,
) -> InferenceRequest {
    let (mut request, _, _) = crate::pipeline::inference::build_request(
        window,
        config,
        si,
        provider,
        stage_name,
        stage_iterations,
        prior,
    );
    let remaining = window.max_tokens.saturating_sub(window.current_tokens);
    request.max_tokens = remaining.min(256); // short routing response
    request.temperature = 0.0; // deterministic routing
    match routing_tool_choice(&si.provider_name) {
        Some(choice) if !request.tools.is_empty() => {
            let mut extra = match request.extra.take() {
                serde_json::Value::Object(map) => map,
                _ => serde_json::Map::new(),
            };
            extra.insert("tool_choice".to_string(), choice);
            request.extra = serde_json::Value::Object(extra);
        }
        _ => request.tools.clear(),
    }
    request
}

/// Transition-choice dispatch: for each `AwaitingTransitionChoice` agent, inject
/// the "which stage next?" prompt into its context, build a short deterministic
/// request, acquire a per-model permit, spawn the inference onto the transition
/// lane, and move it to `AwaitingTransitionResponse`. Provider-missing / pool-full
/// leaves it choosing and retries next tick (same backpressure as
/// [`dispatch_inference`]).
pub(crate) fn dispatch_transition_choice(
    mut agents: Query<TransitionChoiceQuery, With<AwaitingTransitionChoice>>,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    let now = chrono::Utc::now().timestamp();
    for (entity, state, mut window, si, bp, cursor, choice, in_flight, stalled, built_from) in
        agents.iter_mut()
    {
        let (config, progress, prefix, block_prefix, calibration) = built_from;
        crate::tick_scope::enter(entity);
        if state.status != AgentStatus::Active {
            continue; // paused / waiting / cancelled - don't start new work
        }
        // Same bookkeeping as the inference lane: an agent parked here is
        // runnable with nothing outstanding, so a decline that never resolves
        // wedges the run just as thoroughly.
        let Some(provider) = providers.0.get(&si.provider_name) else {
            commands
                .entity(entity)
                .insert(note_stall(stalled, StallReason::ProviderMissing, now));
            continue; // provider not registered - retry later
        };
        let Some(permit) = stage.pools.try_acquire(&si.provider_name, &si.model) else {
            commands
                .entity(entity)
                .insert(note_stall(stalled, StallReason::PoolFull, now));
            continue; // pool full - retry next tick
        };

        let current = &bp.0.stages[cursor.index];
        let prompt = build_transition_prompt(current, &choice.0);
        let tokens = leviath_core::estimate_tokens(&prompt);
        let _ = window.add_typed_entry(
            "conversation",
            leviath_core::EntryKind::UserMessage,
            prompt,
            tokens,
        );

        let request = routing_request(
            &window,
            config,
            si,
            &provider,
            &state.current_stage,
            progress.map(|p| p.iterations).unwrap_or(0),
            crate::pipeline::inference::PriorCalls {
                system_hash: prefix.map(|p| p.0),
                block_hashes: block_prefix.map(|b| b.0.clone()).unwrap_or_default(),
                calibration: calibration.copied(),
                raise_output_cap: false,
            },
        );

        let job = InferenceJob {
            entity,
            provider,
            request,
            permit,
            // The routing request is the stage's own request plus one question,
            // so it is guarded the way the stage's is, from the same correction.
            calibration: calibration.copied(),
            // And buffered for the same reason: a stage name is one short
            // answer that arrives in one piece. Streaming exists here to keep a
            // long silent generation from being mistaken for a dead socket, and
            // this call is never long enough for that to arise.
            stream: false,
        };
        let cancel = crate::cancel::CancelToken::new();
        // Supervised for the same reason as the inference lane: the agent is
        // about to wait on `AwaitingTransitionResponse`, so a job that dies
        // without reporting would strand it mid-route.
        let lost_outcomes = stage.transition_outcomes.clone();
        let lost_wake = stage.wake.clone();
        crate::lane_supervisor::spawn_supervised(
            &stage.runtime,
            "transition-choice",
            run_inference_job(
                job,
                stage.transition_outcomes.clone(),
                stage.wake.clone(),
                crate::inference_bridge::RetryPolicy::default(),
                cancel.clone(),
            ),
            move |message| {
                let _ = lost_outcomes.send(crate::inference_bridge::InferenceOutcome {
                    entity,
                    result: Err(leviath_providers::ProviderError::Other(message)),
                    latency: std::time::Duration::ZERO,
                    // A job that never reached a provider has no rates and no
                    // cost; there is nothing to price.
                    pricing: None,
                });
                lost_wake.notify_one();
            },
        );
        track_in_flight(&mut commands, entity, in_flight, cancel);
        commands
            .entity(entity)
            .remove::<AwaitingTransitionChoice>()
            .remove::<DispatchStall>()
            .insert(AwaitingTransitionResponse(choice.0.clone()));
    }
}

/// What `collect_transition_choice` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type CollectTransitionChoiceQuery = (
    &'static AgentBlueprint,
    &'static mut StageCursor,
    &'static mut AgentState,
    &'static mut StageProgress,
    &'static StageInferences,
    &'static StageSetups,
    &'static mut VisitCounts,
    &'static mut ContextWindow,
    &'static AwaitingTransitionResponse,
    Option<&'static mut crate::persistence::RunOutcomeFlags>,
    Option<&'static crate::persistence::RunMetadata>,
    Option<&'static mut crate::persistence::TokenTotals>,
    Option<&'static mut StageLedger>,
);

/// Transition-choice collect: drain completed routing inferences, match each to a
/// target stage (or completion), record the decision in context, and either enter
/// the chosen stage (loop to `ReadyToInfer`) or mark the agent `Complete`. A
/// provider error marks the agent `Error`.
pub(crate) fn collect_transition_choice(
    mut results: ResMut<TransitionResults>,
    mut agents: Query<CollectTransitionChoiceQuery>,
    sink: Option<Res<crate::host::WorldEventSink>>,
    persist: Option<Res<crate::pipeline::PersistenceStage>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    while let Ok(outcome) = results.0.try_recv() {
        let Ok((
            bp,
            mut cursor,
            mut state,
            mut progress,
            stage_infs,
            setups,
            mut visits,
            mut window,
            resp,
            mut flags,
            metadata,
            mut totals,
            mut ledger,
        )) = agents.get_mut(outcome.entity)
        else {
            continue; // stale: agent cancelled/despawned since dispatch
        };
        crate::tick_scope::enter(outcome.entity);
        // Cancelled/failed mid-choice: every arm below rewrites the status
        // (including a bare `Complete` when nothing matches), which would report
        // a cancelled run as having finished normally.
        if is_terminal_status(&state.status) {
            commands
                .entity(outcome.entity)
                .remove::<AwaitingTransitionResponse>()
                .remove::<InFlightWork>();
            continue;
        }
        // Paused mid-routing. Same hazard as the stage lane: every arm below
        // rewrites the status, so letting a landing outcome through would either
        // route a paused run into its next stage or bury the pause under an
        // `Error`. Park it for `resume` to replay.
        if state.status == AgentStatus::Paused {
            commands
                .entity(outcome.entity)
                .insert(crate::pipeline::HeldInference {
                    outcome,
                    lane: crate::pipeline::HeldLane::TransitionChoice,
                });
            continue;
        }
        let response = match outcome.result {
            Ok(response) => response,
            Err(err) => {
                // An empty account or a network that is down is not this run's
                // fault, and it was not a moment ago either - the same failure
                // one call earlier, on the stage's own inference, parks the run
                // for a resume. Landing it at a stage boundary parks it too:
                // failing here throws away every completed stage over a blip
                // that is usually gone in seconds.
                let provider = &stage_infs.0[cursor.index].provider_name;
                if let Some((blocker, message)) =
                    crate::pipeline::response::setup_park(&err, provider)
                {
                    tracing::warn!(
                        provider = %provider,
                        blocker = %blocker,
                        error = %err,
                        "pausing the run until the machine is fixed"
                    );
                    state.status = AgentStatus::Paused;
                    commands
                        .entity(outcome.entity)
                        .remove::<AwaitingTransitionResponse>()
                        .remove::<InFlightWork>()
                        .insert(crate::pipeline::PausedForSetup {
                            blocker,
                            remedy: message,
                        })
                        // Put the run back where dispatch found it, so a resume
                        // asks for the routing choice again rather than
                        // re-running the stage that already answered. The edges
                        // are the same ones dispatch was handed; `resp` is the
                        // copy it carried through the call.
                        .insert(crate::pipeline::AwaitingTransitionChoice(resp.0.clone()));
                    continue;
                }
                commands
                    .entity(outcome.entity)
                    .remove::<AwaitingTransitionResponse>();
                // The routing call failed, not the stage's work. Fail it through
                // the stage's own `error` edge: `resolve_transition` reads an
                // errored outcome and never comes back here for a second choice.
                crate::pipeline::fail_stage(
                    &mut commands,
                    outcome.entity,
                    &mut state,
                    err.to_string(),
                );
                continue;
            }
        };

        // Routing calls are short but not free, and one fires at every stage
        // boundary of every branching run. Read off the stage's own inference
        // config, which is what dispatch resolved the provider from.
        //
        // Indexed rather than looked up, like the `bp.0.stages[cursor.index]`
        // below it: `StageInferences` is built one entry per stage at spawn, so
        // a cursor that could miss here would already have panicked there.
        //
        // Billed to the stage being left, which is the stage that asked the
        // question. `state.current_stage` is still that stage here: the cursor
        // does not move until `enter_stage` below.
        let si = &stage_infs.0[cursor.index];
        crate::inference_usage::record_call(
            totals.as_deref_mut(),
            ledger.as_deref_mut(),
            persist.as_deref(),
            metadata,
            &crate::inference_usage::CallUsage {
                kind: leviath_core::run_archive::InferenceKind::Routing,
                stage: &state.current_stage,
                iteration: state.iteration,
                provider: &si.provider_name,
                model: &si.model,
                usage: &response.tokens_used,
                pricing: outcome.pricing,
            },
        );

        let choice = response.content.trim().to_string();
        let tokens = leviath_core::estimate_tokens(&choice);
        let _ = window.add_typed_entry(
            "conversation",
            leviath_core::EntryKind::AssistantTurn { tool_calls: vec![] },
            format!("Transitioning to: {choice}"),
            tokens,
        );

        let allow_complete = bp.0.stages[cursor.index].allow_complete;
        match match_transition_choice(&choice, &resp.0, allow_complete) {
            Some(target) => {
                let idx =
                    bp.0.stages
                        .iter()
                        .position(|s| s.name == target)
                        .unwrap_or(0);
                // The chosen edge (absent when the matched target has no explicit
                // edge, e.g. a fallback - then Direct, ungated).
                let edge = resp.0.iter().find(|e| e.target == target);
                let transform = edge.map(|e| e.transform.clone()).unwrap_or_default();
                // The edge's gate is checked BEFORE its transform runs, so a
                // held stage keeps the context it still needs.
                let stage = &bp.0.stages[cursor.index];
                match gate_blocks(
                    edge.and_then(|e| e.gate.as_ref()),
                    stage,
                    &progress,
                    &window,
                ) {
                    GateDecision::Block(nudge) => {
                        hold_for_gate(
                            outcome.entity,
                            &nudge,
                            &mut progress,
                            &mut window,
                            &mut commands,
                        );
                        continue;
                    }
                    GateDecision::Forced => {
                        if let Some(flags) = flags.as_mut() {
                            flags.0.gates_forced += 1;
                        }
                    }
                    GateDecision::Pass => {}
                }
                let to_compact = apply_edge_transform(&mut window, &transform);
                let setup = &setups.0[idx];
                let from = state.current_stage.clone();
                match enter_stage(
                    idx,
                    &bp.0,
                    setup,
                    StageEntry {
                        cursor: &mut cursor,
                        state: &mut state,
                        progress: &mut progress,
                        visits: &mut visits,
                        window: &mut window,
                        ledger: ledger.as_deref_mut(),
                    },
                ) {
                    Ok(visit) => {
                        // No `status = Active` here, unlike the same sequence in
                        // `resolve_transition`. That reset exists to clear an
                        // error status when recovering down an `error` edge, and
                        // this path cannot be carrying one: `StageResolution`
                        // only yields `Choose` from the branch that ran with no
                        // stage outcome, so an errored stage routes to `Next`
                        // and never reaches an LLM choice.
                        let name = bp.0.stages[idx].name.clone();
                        emit_stage_transition(&sink, metadata, &state.agent_id, from, &name, visit);
                        let mut ec = commands.entity(outcome.entity);
                        ec.remove::<AwaitingTransitionResponse>();
                        attach_stage_components(ec, stage_infs.0[idx].clone(), setup, idx, name);
                        if !to_compact.is_empty() {
                            commands
                                .entity(outcome.entity)
                                .insert(PendingEdgeCompact(to_compact));
                        }
                    }
                    Err(message) => {
                        commands
                            .entity(outcome.entity)
                            .remove::<AwaitingTransitionResponse>();
                        crate::pipeline::fail_stage(
                            &mut commands,
                            outcome.entity,
                            &mut state,
                            message,
                        );
                    }
                }
            }
            None => {
                state.status = AgentStatus::Complete;
                commands
                    .entity(outcome.entity)
                    .remove::<AwaitingTransitionResponse>();
            }
        }
    }
}
