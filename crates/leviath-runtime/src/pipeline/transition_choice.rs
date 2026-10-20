//! The LLM-chosen transition: build the prompt that asks which stage runs
//! next, dispatch it as an inference, and match the answer back to one of the
//! edges that were offered.

use super::*;
use crate::insert::RunSpecC;
use crate::spec::graph::{EdgeDef, StageDef};
use spec_view::StageToolOverrides;

/// A transition-choice inference is in flight (an LLM is picking the next stage);
/// holds the choosable edges so the collect system can match the response back to
/// one.
#[derive(Component, Debug, Clone)]
pub(crate) struct AwaitingTransitionResponse(pub Vec<EdgeDef>);

/// The receiving end of the transition-choice outcomes channel, as a world
/// resource for the collect system. (The sending end lives in
/// [`InferenceStage::transition_outcomes`].)
#[derive(Resource)]
pub(crate) struct TransitionResults(pub UnboundedReceiver<InferenceOutcome>);

/// Build the LLM prompt that asks which stage to run next.
pub(crate) fn build_transition_prompt(stage: &StageDef, edges: &[EdgeDef]) -> String {
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
        p.push_str(&format!("- {}", edge.to));
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

/// [`route_choice`] without the reason.
#[cfg(test)]
pub(crate) fn match_transition_choice(
    choice: &str,
    edges: &[EdgeDef],
    allow_complete: bool,
) -> Option<String> {
    route_choice(choice, edges, allow_complete).map(|(to, _)| to)
}

/// Where a routing answer sends the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Routed {
    /// Into this stage, for this reason.
    To(String, crate::state::TransitionReason),
    /// Nowhere: the run ends here.
    End,
}

/// [`named_choice`], falling back to [`fallback_choice`] when the answer
/// names nothing, with `None` for the end of the run.
#[cfg(test)]
pub(crate) fn route_choice(
    choice: &str,
    edges: &[EdgeDef],
    allow_complete: bool,
) -> Option<(String, crate::state::TransitionReason)> {
    match named_choice(choice, edges, allow_complete)
        .unwrap_or_else(|| fallback_choice(edges, allow_complete))
    {
        Routed::To(to, reason) => Some((to, reason)),
        Routed::End => None,
    }
}

/// Match an LLM transition response to one of the choosable edges' target
/// stages, or to `DONE` where the stage may complete. `None` when it names
/// neither.
///
/// Models are asked to answer with only the target stage name (or `DONE`), but
/// frequently wrap it in prose or re-explain the stage. We therefore look for a
/// clean, standalone decision - scanning the first line, then the concluding
/// line, for a **whole-word** match against a stage name or `DONE` - instead of
/// substring-scanning the whole response, where a stage name mentioned in
/// passing ("the implementation", "the approved plan") would hijack the routing.
pub(crate) fn named_choice(
    choice: &str,
    edges: &[EdgeDef],
    allow_complete: bool,
) -> Option<Routed> {
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
        line.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
            .filter(|w| !w.is_empty())
            .count()
    };
    let first = lines.first().copied();
    let last = lines
        .last()
        .copied()
        .filter(|l| lines.len() > 1 && words_in(l) <= 3);
    let mut words = first
        .into_iter()
        .chain(last)
        .flat_map(|line| line.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-'));
    words.find_map(|word| {
        if allow_complete && word.eq_ignore_ascii_case("done") {
            return Some(Routed::End);
        }
        edges
            .iter()
            .find(|e| word.eq_ignore_ascii_case(e.to.as_str()))
            .map(|edge| {
                Routed::To(
                    edge.to.to_string(),
                    crate::state::TransitionReason::ModelChoice,
                )
            })
    })
}

/// Where the run goes when the answer named nothing it could take: a stage
/// that may end prefers ending over looping back; otherwise the run advances
/// along the first declared edge.
pub(crate) fn fallback_choice(edges: &[EdgeDef], allow_complete: bool) -> Routed {
    match edges.first() {
        Some(edge) if !allow_complete => Routed::To(
            edge.to.to_string(),
            crate::state::TransitionReason::Fallback,
        ),
        _ => Routed::End,
    }
}

/// The routing question was asked once more, because the first answer named
/// no stage. A second answer that names none takes [`fallback_choice`].
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct RoutingReasked;

/// The question asked when the first routing answer named no stage: the
/// stage names alone, and `DONE` where the stage may end the run.
pub(crate) fn build_reask_prompt(edges: &[EdgeDef], allow_complete: bool) -> String {
    let mut names: Vec<&str> = edges.iter().map(|e| e.to.as_str()).collect();
    if allow_complete {
        names.push("DONE");
    }
    let mut p = format!(
        "Your last reply named no stage. Answer with exactly one of: {}.",
        names.join(", ")
    );
    if allow_complete {
        p.push_str(" DONE ends the run here.");
    }
    p
}

/// The most characters of a reply quoted in a routing note.
const QUOTED_REPLY_CHARS: usize = 80;

/// Why a routing reply named no stage, as a stage log reads it.
fn why_unrouted(response: &leviath_providers::InferenceResponse, answer: &str) -> String {
    if response.finish_reason == leviath_providers::FinishReason::TokenLimit {
        return "the reply was cut off before it named a stage".to_string();
    }
    if response.content_is_reasoning {
        return "the reply was the model's reasoning, with no answer".to_string();
    }
    if answer.is_empty() {
        return "the answer was empty".to_string();
    }
    let flat = answer.split_whitespace().collect::<Vec<_>>().join(" ");
    let quoted: String = flat.chars().take(QUOTED_REPLY_CHARS).collect();
    let more = if quoted.len() < flat.len() { "..." } else { "" };
    format!("the answer named no stage (\"{quoted}{more}\")")
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
    &'static RunSpecC,
    &'static StageCursor,
    &'static AwaitingTransitionChoice,
    Option<&'static InFlightWork>,
    Option<&'static DispatchStall>,
    Option<&'static RoutingReasked>,
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

/// Model id prefixes whose upstream takes only `tool_choice: "auto"`.
///
/// Meta's API refuses `"none"` with a 400, directly and as OpenRouter's
/// upstream for `meta/*` models. OpenRouter's catalogue still lists
/// `tool_choice` for those models, because `"auto"` works, so the listing
/// alone cannot tell; this list answers for them before it is asked.
const AUTO_ONLY_TOOL_CHOICE: &[&str] = &["meta/"];

/// Provider and model pairs that refused a routing request's `tool_choice`
/// in this process, so later routing calls to them go out with no tools at
/// all rather than being refused again first.
static REFUSED_TOOL_CHOICE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<(String, String)>>,
> = std::sync::LazyLock::new(Default::default);

/// Remember that `provider` refused a routing `tool_choice` for `model`.
pub(crate) fn remember_refused_tool_choice(provider: &str, model: &str) {
    leviath_core::sync::lock(&REFUSED_TOOL_CHOICE)
        .insert((provider.to_string(), model.to_string()));
}

/// Whether `provider` refused a routing `tool_choice` for `model` earlier in
/// this process.
pub(crate) fn refused_tool_choice(provider: &str, model: &str) -> bool {
    leviath_core::sync::lock(&REFUSED_TOOL_CHOICE)
        .contains(&(provider.to_string(), model.to_string()))
}

/// Whether a failed call was the provider refusing the request's
/// `tool_choice`: a 400 whose body names the parameter.
pub(crate) fn refuses_tool_choice(error: &leviath_providers::ProviderError) -> bool {
    let text = error.to_string();
    let bad_request = error.failure_kind() == Some(leviath_providers::FailureKind::BadRequest)
        || text.contains("HTTP 400");
    bad_request && text.contains("tool_choice")
}

/// `request` as a routing request with no tools and no `tool_choice`: the
/// form every provider takes, at the cost of a cold prefix.
pub(crate) fn without_tool_choice(request: &InferenceRequest) -> InferenceRequest {
    let mut request = request.clone();
    request.tools.clear();
    request.extra.as_object_mut().into_iter().for_each(|extra| {
        extra.remove("tool_choice");
    });
    request
}

/// The most a routing answer may spend.
///
/// The answer is one stage name, but a model that reasons spends output
/// tokens thinking before it writes it, and at 256 the budget ran out first:
/// what came back was the working-out, cut off. This only bounds the worst
/// case. A one-word answer still costs one word.
pub(crate) const ROUTING_ANSWER_TOKENS: usize = 2048;

/// OpenRouter model id prefixes that Anthropic serves, whose cached messages
/// a change to the thinking settings throws away.
const ANTHROPIC_UPSTREAM: &[&str] = &["anthropic/", "~anthropic/"];

/// The reasoning control a routing request carries, as request extras, or
/// `Value::Null` for none.
///
/// On a route whose every model reasons, the least effort the route takes
/// (see [`crate::title::least_reasoning_extra`]). On OpenRouter, the least
/// effort a model that must reason takes, by the same rule the title call
/// uses ([`crate::reasoning_floor::least_effort`]); a model that need not
/// reason gets nothing, since `enabled: false` is refused by some models and
/// a refused routing call stops the run.
///
/// The Responses routes cache the input alone, so the effort leaves the
/// cached prefix as it was. Anthropic counts a change to the thinking
/// settings against the cached messages, and the stage request the routing
/// call follows sent none. So Anthropic's models get nothing here, directly,
/// on Bedrock or through OpenRouter, even where the catalogue says they must
/// reason. They rely on [`ROUTING_ANSWER_TOKENS`].
fn routing_reasoning(
    provider_name: &str,
    provider: &dyn Provider,
    model: &str,
) -> serde_json::Value {
    match provider_name {
        "openrouter" => {
            let id = crate::reasoning_floor::listed_model(provider, model)
                .map_or_else(|| model.to_string(), |(id, _)| id);
            let anthropic = ANTHROPIC_UPSTREAM.iter().any(|p| id.starts_with(p));
            (!anthropic)
                .then(|| crate::reasoning_floor::least_effort(provider, model))
                .flatten()
                .map_or(
                    serde_json::Value::Null,
                    |effort| serde_json::json!({ "reasoning": { "effort": effort } }),
                )
        }
        _ => crate::title::least_reasoning_extra(provider_name),
    }
}

/// The answer a routing reply gives, or `None` when it gives none.
///
/// A reply the output cap cut off is unfinished, and a reply whose text is
/// the model's reasoning standing in for an empty answer is working-out.
/// Either can name a stage in passing, so neither is read as a choice.
pub(crate) fn routing_answer(response: &leviath_providers::InferenceResponse) -> Option<&str> {
    let unfinished = response.finish_reason == leviath_providers::FinishReason::TokenLimit;
    (!unfinished && !response.content_is_reasoning).then(|| response.content.trim())
}

/// Whether OpenRouter will forward `tool_choice: "none"` for `model`.
///
/// A model in an [`AUTO_ONLY_TOOL_CHOICE`] family never does. Otherwise the
/// catalogue's `supported_parameters` decides when it was read and lists the
/// model. A model it does not describe is sent `"none"`, which most of the
/// gateway's upstreams take; a refusal is caught and remembered by the call
/// itself.
fn gateway_takes_none(provider: &dyn Provider, model: &str) -> bool {
    let listed = crate::reasoning_floor::listed_model(provider, model);
    let id = listed.as_ref().map_or(model, |(id, _)| id.as_str());
    let auto_only = AUTO_ONLY_TOOL_CHOICE.iter().any(|p| id.starts_with(p));
    !auto_only && listed.and_then(|(_, entry)| entry.supports_tool_choice) != Some(false)
}

/// The shape of `tool_choice: none` for a provider's wire format, or `None`
/// when the routing request should carry no tools at all.
///
/// The routing request carries the stage's tool list so that its prefix is
/// the stage request's prefix - every provider caches the tool definitions
/// as part of it - and this is what stops the model from answering the
/// "which stage next?" question with a tool call instead of a name. A
/// provider this cannot vouch for gets a request with no tools at all,
/// which is a cold prefix but a safe one. So does Meta, whose API takes
/// only `"auto"`, and any provider and model that refused a `tool_choice`
/// earlier in this process.
fn routing_tool_choice(
    provider_name: &str,
    provider: &dyn Provider,
    model: &str,
) -> Option<serde_json::Value> {
    if refused_tool_choice(provider.name(), model) {
        return None;
    }
    match provider_name {
        "anthropic" => Some(serde_json::json!({ "type": "none" })),
        // The Responses providers (Codex, xAI and Grok) take the same bare
        // string. Without an arm here the fallback clears the tool array
        // instead, which cold-reads the whole window on every stage
        // transition.
        "openai" | "gemini" | "codex" | "xai" | "grok" => Some(serde_json::json!("none")),
        "openrouter" => gateway_takes_none(provider, model).then(|| serde_json::json!("none")),
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
    request.max_tokens = remaining.min(ROUTING_ANSWER_TOKENS);
    request.temperature = 0.0; // deterministic routing
    let mut extra = match request.extra.take() {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    match routing_tool_choice(&si.provider_name, provider.as_ref(), &request.model) {
        Some(choice) if !request.tools.is_empty() => {
            extra.insert("tool_choice".to_string(), choice);
        }
        _ => request.tools.clear(),
    }
    if let serde_json::Value::Object(least) =
        routing_reasoning(&si.provider_name, provider.as_ref(), &request.model)
    {
        extra.extend(least);
    }
    request.extra = match extra.is_empty() {
        true => serde_json::Value::Null,
        false => serde_json::Value::Object(extra),
    };
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
    retry: Option<Res<crate::pipeline::InferenceRetryTuning>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    let now = chrono::Utc::now().timestamp();
    // The daemon inserts this from `[limits]`; a world that never set it gets
    // the built-in schedule.
    let tuning = retry.map(|r| *r).unwrap_or_default();
    for (
        entity,
        state,
        mut window,
        si,
        spec,
        cursor,
        choice,
        in_flight,
        stalled,
        reasked,
        built_from,
    ) in agents.iter_mut()
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

        let current = &spec.0.graph.stages[cursor.index];
        // Asked once more, the question is the stage names alone: the full
        // prompt is still in the context just above.
        let prompt = match reasked {
            Some(_) => build_reask_prompt(&choice.0, current.allow_complete),
            None => build_transition_prompt(current, &choice.0),
        };
        let tokens = leviath_core::estimate_tokens(&prompt);
        let _ = window.add_typed_entry_caused(
            leviath_core::ContextCause::Framework,
            "conversation",
            leviath_core::EntryKind::UserMessage,
            prompt,
            tokens,
        );

        let mut request = routing_request(
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

        // The routing question carries the stage's context, so it is held to
        // the same retention rule and sent the same request fields.
        providers
            .0
            .apply_retention_knobs(&si.provider_name, &mut request.extra);
        let job = InferenceJob {
            jobs: None,
            entity,
            refused: providers.0.retention_refusal(&si.provider_name, &si.model),
            provider,
            request,
            permit,
            // The routing request is the stage's own request plus one question,
            // so it is guarded the way the stage's is, from the same correction.
            calibration: calibration.copied(),
            // Buffered: the answer is one stage name. A model that reasons may
            // think for up to `ROUTING_ANSWER_TOKENS` first, which can take tens
            // of seconds with nothing on the wire. The request keeps the
            // stage's own timeout (900 seconds unless the stage sets one), and
            // no idle-read limit applies, so that silence is not taken for a
            // dead socket.
            stream: false,
            hydration: None,
            // The routing question's attempts are not journaled, and the stage
            // lane's are. A routing call is one deterministic sentence with a
            // small budget and no fallbacks to move to, so its retries are
            // measured in seconds and the run's provider never changes under it.
            // An attempt record exists to explain a turn that took a long time
            // or ended up on a different model, and neither is a thing this call
            // does.
            journal: None,
        };
        // Held on the agent as a call like the stage's own, so the agent waits
        // on `AwaitingTransitionResponse` while the world decides on each
        // failed trip; the trips themselves are supervised.
        // The stage lane's schedule, with the routing call's own number of
        // tries: the stage's, then the graph's, then the operator's. A
        // question asked once more is a new call and gets them all again.
        let attempts = crate::spec::graph::RoutingDef::attempts(
            spec.0.graph.routing.as_ref(),
            current.routing.as_ref(),
            tuning.routing_attempts,
        );
        let policy = crate::inference_bridge::RetryPolicy {
            max_attempts: attempts,
            ..crate::pipeline::retry_policy_for(config, tuning)
        };
        let (call, cancel) = crate::inference_call::start_call(
            &stage,
            job,
            policy,
            crate::inference_call::CallLane::Routing,
        );
        track_in_flight(&mut commands, entity, in_flight, cancel);
        commands
            .entity(entity)
            .insert(call)
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
    &'static RunSpecC,
    &'static mut StageCursor,
    &'static mut AgentState,
    &'static mut StageProgress,
    Option<&'static StageToolOverrides>,
    &'static mut VisitCounts,
    &'static mut ContextWindow,
    &'static AwaitingTransitionResponse,
    Option<&'static mut crate::persistence::RunOutcomeFlags>,
    Option<&'static crate::persistence::RunMetadata>,
    Option<&'static mut crate::persistence::TokenTotals>,
    Option<&'static mut StageLedger>,
    (
        Option<&'static StageInference>,
        Option<&'static RoutingReasked>,
    ),
    Option<&'static mut crate::pipeline::StageIoBuffer>,
);

/// Transition-choice collect: drain completed routing inferences, match each to a
/// target stage (or completion), record the decision in context, and either enter
/// the chosen stage (loop to `ReadyToInfer`) or mark the agent `Complete`. A
/// provider error marks the agent `Error`.
pub(crate) fn collect_transition_choice(
    mut results: ResMut<TransitionResults>,
    mut agents: Query<CollectTransitionChoiceQuery>,
    mut calls: crate::inference_call::CallParams,
    sink: Option<Res<crate::host::WorldEventSink>>,
    persist: Option<Res<crate::pipeline::JournalSender>>,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    while let Ok(mut outcome) = results.0.try_recv() {
        let Ok((
            spec,
            mut cursor,
            mut state,
            mut progress,
            overrides,
            mut visits,
            mut window,
            resp,
            mut flags,
            metadata,
            mut totals,
            mut ledger,
            (called, reasked),
            mut buffer,
        )) = agents.get_mut(outcome.entity)
        else {
            continue; // stale: agent cancelled/despawned since dispatch
        };
        crate::tick_scope::enter(outcome.entity);
        let graph = &spec.0.graph;
        // Cancelled/failed mid-choice: every arm below rewrites the status
        // (including a bare `Complete` when nothing matches), which would report
        // a cancelled run as having finished normally.
        if is_terminal_status(&state.status) {
            commands
                .entity(outcome.entity)
                .remove::<AwaitingTransitionResponse>()
                .remove::<InFlightWork>()
                .remove::<crate::inference_call::InferenceCall>();
            continue;
        }
        // A failed trip the call will make again: the agent keeps waiting.
        if calls.settle(&mut outcome, &mut commands) != crate::inference_call::Next::Done {
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
                //
                // The provider named is the one this call went to: the live
                // component, which a failover earlier in the stage moved on
                // from the one the stage resolved to.
                let planned = spec_view::stage_inference(&spec.0, cursor.index, overrides);
                let provider = called
                    .map(|si| si.provider_name.as_str())
                    .unwrap_or(&planned.provider_name);
                if let Some((blocker, message)) = crate::pipeline::park::setup_park(&err, provider)
                {
                    tracing::warn!(
                        provider = %provider,
                        blocker = %blocker,
                        error = %err,
                        "pausing the run until the machine is fixed"
                    );
                    if let Some(mut buffer) = buffer {
                        buffer
                            .logs
                            .push((cursor.index, format!("[paused] {message}")));
                    }
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
                    .remove::<AwaitingTransitionResponse>()
                    .remove::<RoutingReasked>();
                // The routing call failed, not the stage's work, and its tries
                // are spent. By default the run stops here with that error:
                // following the stage's `error` edge would redo work that is
                // finished, and a provider that refuses the routing question
                // refuses it again on the way back, so the run circles until
                // the stage's revisits run out. A blueprint that wants the
                // edge anyway says so with `routing.on_error = "error_edge"`.
                let stage = &graph.stages[cursor.index];
                let on_error = crate::spec::graph::RoutingDef::on_error(
                    graph.routing.as_ref(),
                    stage.routing.as_ref(),
                );
                match on_error {
                    crate::spec::graph::OnRoutingError::ErrorEdge => {
                        // `resolve_transition` reads an errored outcome and
                        // never comes back here for a second choice.
                        crate::pipeline::fail_stage(
                            &mut commands,
                            outcome.entity,
                            &mut state,
                            err.to_string(),
                        );
                    }
                    crate::spec::graph::OnRoutingError::Fail => {
                        let message = format!(
                            "the call that picks the stage after '{}' failed: {err}. The \
                             stage's work is kept and its error edge was not taken; set \
                             routing.on_error = \"error_edge\" to take it",
                            stage.name
                        );
                        if let Some(mut buffer) = buffer {
                            buffer
                                .logs
                                .push((cursor.index, format!("[routing] {message}")));
                        }
                        state.status = AgentStatus::Error { message };
                    }
                }
                continue;
            }
        };

        // Routing calls are short but not free, and one fires at every stage
        // boundary of every branching run. Read off the stage's own inference
        // config, which is what dispatch resolved the provider from.
        //
        // Billed to the stage being left, which is the stage that asked the
        // question. `state.current_stage` is still that stage here: the cursor
        // does not move until `enter_stage` below.
        let si = spec_view::stage_inference(&spec.0, cursor.index, overrides);
        crate::inference_usage::record_call(
            totals.as_deref_mut(),
            ledger.as_deref_mut(),
            persist.as_deref(),
            metadata,
            &crate::inference_usage::CallUsage {
                kind: crate::runfile::record::InferenceKind::Routing,
                stage: &state.current_stage,
                iteration: state.iteration,
                provider: &si.provider_name,
                model: &si.model,
                usage: &response.tokens_used,
                pricing: outcome.pricing,
            },
        );

        // A reply that gives no answer is read as an empty one.
        let choice = routing_answer(&response).unwrap_or_default().to_string();
        let allow_complete = graph.stages[cursor.index].allow_complete;
        let named = named_choice(&choice, &resp.0, allow_complete);
        // The framework's own phrasing of the decision, not the model's text:
        // the reply was a bare stage name. A reply that named none is kept as
        // it came, so a second ask reads what it is answering.
        let entry = match (&named, choice.is_empty()) {
            (Some(_), _) => format!("Transitioning to: {choice}"),
            (None, false) => choice.clone(),
            (None, true) => "(no answer)".to_string(),
        };
        let tokens = leviath_core::estimate_tokens(&entry);
        let _ = window.add_typed_entry_caused(
            leviath_core::ContextCause::Framework,
            "conversation",
            leviath_core::EntryKind::AssistantTurn { tool_calls: vec![] },
            entry,
            tokens,
        );
        // This answer settles the question one way or another, or asks it
        // again below.
        commands.entity(outcome.entity).remove::<RoutingReasked>();
        let routed = match named {
            Some(routed) => routed,
            None => {
                let stage = &graph.stages[cursor.index];
                let why = why_unrouted(&response, &choice);
                // Asked once more before guessing, on a new routing call with
                // its own tries. The stage's retries and the call's tries are
                // for failures, and this call did not fail.
                if reasked.is_none() {
                    let note = format!("{why}; asking once more");
                    tracing::warn!(stage = %stage.name, "routing: {note}");
                    if let Some(buffer) = buffer.as_mut() {
                        buffer
                            .logs
                            .push((cursor.index, format!("[routing] {note}")));
                    }
                    commands
                        .entity(outcome.entity)
                        .remove::<AwaitingTransitionResponse>()
                        .insert(crate::pipeline::AwaitingTransitionChoice(resp.0.clone()))
                        .insert(RoutingReasked);
                    continue;
                }
                let fallback = fallback_choice(&resp.0, allow_complete);
                let took = match &fallback {
                    Routed::To(to, _) => format!("took '{to}', the first edge"),
                    Routed::End => "ended the run, as the stage may".to_string(),
                };
                let note = format!("{why}, asked twice; {took}");
                tracing::warn!(stage = %stage.name, "routing: {note}");
                if let Some(buffer) = buffer.as_mut() {
                    buffer
                        .logs
                        .push((cursor.index, format!("[routing] {note}")));
                }
                fallback
            }
        };

        match routed {
            Routed::To(target, picked) => {
                let idx = spec_view::stage_index(graph, &target).unwrap_or(0);
                // The chosen edge (absent when the matched target has no explicit
                // edge, e.g. a fallback - then Direct, ungated).
                let edge = resp.0.iter().find(|e| e.to.as_str() == target);
                let transform = edge.map(|e| e.carry.clone()).unwrap_or_default();
                // The edge's gate is checked BEFORE its transform runs, so a
                // held stage keeps the context it still needs.
                let stage = &graph.stages[cursor.index];
                let mut reason = picked;
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
                        reason = crate::state::TransitionReason::Gate;
                        if let Some(flags) = flags.as_mut() {
                            flags.0.gates_forced += 1;
                        }
                    }
                    GateDecision::Pass => {}
                }
                let to_compact = apply_edge_transform(&mut window, &transform);
                let setup = spec_view::stage_setup(&spec.0, idx);
                let from = state.current_stage.clone();
                let edge_name = edge.map(|e| e.name.clone());
                match enter_stage(
                    idx,
                    graph,
                    &setup,
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
                        let name = graph.stages[idx].name.to_string();
                        let how = (edge_name.as_ref(), reason);
                        emit_stage_transition(
                            &sink,
                            metadata,
                            &state.agent_id,
                            from.clone(),
                            &name,
                            visit,
                            how,
                        );
                        let taken = transition_record(&from, &state, edge_name, reason);
                        super::transition::journal_transition(
                            persist.as_deref(),
                            metadata,
                            taken.as_ref(),
                        );
                        let mut ec = commands.entity(outcome.entity);
                        ec.remove::<AwaitingTransitionResponse>();
                        taken.into_iter().for_each(|t| {
                            ec.insert(t);
                        });
                        let inference = spec_view::stage_inference(&spec.0, idx, overrides);
                        attach_stage_components(ec, inference, &setup, idx, name);

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
            Routed::End => {
                state.status = AgentStatus::Complete;
                commands
                    .entity(outcome.entity)
                    .remove::<AwaitingTransitionResponse>();
            }
        }
    }
}

#[cfg(test)]
#[path = "transition_choice_tests.rs"]
mod tests;
