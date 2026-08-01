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
    }
}

/// Inference-collect system: drain completed inferences and apply them. A
/// success is stored on the agent (bumping its iteration) and the agent advances
/// to `ProcessResponse`; an error marks the agent `Error`. An outcome for an
/// agent that is no longer `AwaitingInference` (cancelled or despawned between
/// dispatch and now) is dropped.
#[allow(clippy::type_complexity)]
pub fn collect_inference(
    mut results: ResMut<InferenceResults>,
    mut agents: Query<
        (
            &mut AgentState,
            Option<&mut crate::persistence::TokenTotals>,
            Option<&StageCursor>,
            Option<&mut StageLedger>,
            Option<&mut StageIoBuffer>,
            Option<&StageInference>,
            Option<&mut crate::telemetry::StageActivity>,
        ),
        With<AwaitingInference>,
    >,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    while let Ok(outcome) = results.0.try_recv() {
        let Ok((mut state, totals, cursor, mut ledger, buffer, inference, activity)) =
            agents.get_mut(outcome.entity)
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
        let idx = cursor.map_or(0, |c| c.index);
        // Record the call for the telemetry observer while the provider and
        // timing are still at hand (the observer only sees components).
        if let Some(mut activity) = activity {
            let usage = outcome.result.as_ref().ok().map(|r| &r.tokens_used);
            activity
                .0
                .push(crate::telemetry::ActivityRecord::Inference {
                    provider: inference
                        .map(|i| i.provider_name.clone())
                        .unwrap_or_default(),
                    model: inference.map(|i| i.model.clone()).unwrap_or_default(),
                    latency_ms: u64::try_from(outcome.latency.as_millis()).unwrap_or(u64::MAX),
                    prompt_tokens: usage.map_or(0, |u| u.prompt_tokens),
                    completion_tokens: usage.map_or(0, |u| u.completion_tokens),
                    cached_tokens: usage.map_or(0, |u| u.cached_tokens),
                    success: outcome.result.is_ok(),
                });
        }
        match outcome.result {
            Ok(response) => {
                state.iteration += 1;
                if let Some(mut totals) = totals {
                    totals.add_usage(&response.tokens_used);
                }
                // Accrue this iteration's tokens against the current stage record.
                if let Some(rec) = ledger.as_deref_mut().and_then(|l| l.0.get_mut(idx)) {
                    rec.prompt_tokens += response.tokens_used.prompt_tokens;
                    rec.completion_tokens += response.tokens_used.completion_tokens;
                    rec.cached_tokens += response.tokens_used.cached_tokens;
                }
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

/// Per-stage progress counters, reset when an agent enters a stage.
#[derive(Component, Debug, Clone, Default)]
pub struct StageProgress {
    /// Total tool calls the agent has made in this stage.
    pub total_tool_calls: usize,
    /// Consecutive text-only responses that were nudged toward tool use.
    pub text_only_nudges: usize,
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

/// Process-response system: route each `ProcessResponse` agent by whether its
/// last inference asked for tools. Tool calls present ⇒ `ReadyForTools` (and the
/// stage's running tool-call count is bumped); none ⇒ `ReadyForTransition`. Pure
/// routing - no I/O.
#[allow(clippy::type_complexity)]
pub fn process_response(
    mut agents: Query<
        (
            Entity,
            &crate::components::InferenceResult,
            &mut StageProgress,
            Option<&mut crate::persistence::TokenTotals>,
        ),
        With<ProcessResponse>,
    >,
    mut commands: Commands,
) {
    crate::tick_scope::clear();
    for (entity, result, mut progress, totals) in agents.iter_mut() {
        crate::tick_scope::enter(entity);
        progress.iterations += 1; // per-stage inference count (for max_iterations)
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
#[allow(clippy::type_complexity)]
pub fn handle_empty_response(
    mut agents: Query<
        (
            Entity,
            &mut ContextWindow,
            &crate::components::InferenceResult,
            &mut StageProgress,
            &AgentBlueprint,
            &StageCursor,
            Option<&GlobalNudge>,
        ),
        With<ReadyForTransition>,
    >,
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
        if progress.total_tool_calls > 0 || !nudge.enabled || progress.text_only_nudges >= nudge.max
        {
            commands
                .entity(entity)
                .remove::<ReadyForTransition>()
                .insert(ResolveTransition);
        } else {
            progress.text_only_nudges += 1;
            let response_tokens = leviath_core::estimate_tokens(&infer.response);
            let _ = window.add_typed_entry(
                "conversation",
                leviath_core::EntryKind::AssistantTurn { tool_calls: vec![] },
                infer.response.clone(),
                response_tokens,
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
