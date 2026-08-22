//! Tool dispatch: batching, policy triage, and handing calls to the tool lane.

use super::*;

/// The agent's tool batch has been handed to the tool lane; it is waiting for
/// the results (which the tool-collect system will apply).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AwaitingTools;

/// Marker: this agent's advertised tools should be re-resolved before its next
/// turn - mid-run dynamic tool discovery. Consumed by
/// [`refresh_advertised_tools`], which asks the [`ToolService`] for the stage's
/// fresh tool defs and writes them into the live [`StageInference`].
#[derive(Component, Debug, Clone, Copy)]
pub struct ToolsNeedRefresh;

/// Marker: this agent opted into `dynamic_tools`. Only such agents
/// are polled by [`poll_dynamic_tool_refresh`] for a pending tool re-scan, so the
/// default (static) agent pays nothing.
#[derive(Component, Debug, Clone, Copy)]
pub struct DynamicTools;

/// Reports one tool call's result the moment it resolves, from inside the
/// executor - `(tool_call_id, result)`. Dispatch builds one per batch to journal
/// each completion as a `ToolCallDone` record, so a crash mid-batch loses only
/// the calls that genuinely never finished (issue #96). Implementors that don't
/// journal get a no-op.
pub type ToolProgress = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// A [`ToolProgress`] that reports nowhere - for worlds without a persistence
/// lane and for `ToolService` impls under test.
pub fn noop_progress() -> ToolProgress {
    Arc::new(|_, _| {})
}

/// Provides a per-agent tool-execution closure. The concrete implementation
/// (in the CLI) holds each agent's tool registry, workdir, and permission
/// policy; the pipeline stays agnostic to *how* tools run. `exec_for` returns a
/// boxed closure the tool worker runs off the tick.
pub trait ToolService: Send + Sync {
    /// Build the closure that runs `calls` for `entity`, resolving `(id, result)`
    /// pairs. The executor calls `progress` with each call's result as it
    /// resolves (per-call, not at batch end).
    fn exec_for(
        &self,
        entity: Entity,
        calls: Vec<leviath_providers::ToolCall>,
        progress: ToolProgress,
    ) -> BoxedToolExec;

    /// Notify the service that `entity` entered the stage at `stage_index` named
    /// `stage_name`, so it can re-sync that agent's per-stage tool permissions.
    /// Default no-op for services without per-stage policy.
    fn sync_stage(&self, _entity: Entity, _stage_index: usize, _stage_name: &str) {}

    /// Re-resolve `entity`'s advertised tool defs for the stage at `stage_index` -
    /// e.g. after new tools were discovered on disk. `None` means "no change"
    /// (the default, for services without dynamic tools); `Some(tools)` replaces
    /// the stage's advertised set.
    fn refresh_tools(
        &self,
        _entity: Entity,
        _stage_index: usize,
    ) -> Option<Vec<leviath_providers::Tool>> {
        None
    }

    /// Whether `entity` (a `dynamic_tools` agent) has pending tool changes that
    /// warrant a re-scan + re-advertise. Polled by [`poll_dynamic_tool_refresh`];
    /// implementors return (and clear) a per-agent dirty flag. Default `false`.
    fn wants_refresh(&self, _entity: Entity) -> bool {
        false
    }
}

/// The tool service, as a world resource.
#[derive(Resource, Clone)]
pub struct ToolServiceRes(pub Arc<dyn ToolService>);

/// The job sender feeding the tool lane, as a world resource, paired with the
/// lane's occupancy counters so dispatch can record what it queued.
#[derive(Resource, Clone)]
pub struct ToolStage {
    /// Where batches are handed to the lane.
    pub jobs: UnboundedSender<ToolJob>,
    /// Shared with the lane's workers; see [`crate::tool_bridge::ToolLaneStats`].
    pub stats: Arc<crate::tool_bridge::ToolLaneStats>,
}

impl ToolStage {
    /// A stage wired to a real lane's counters.
    pub fn new(
        jobs: UnboundedSender<ToolJob>,
        stats: Arc<crate::tool_bridge::ToolLaneStats>,
    ) -> Self {
        Self { jobs, stats }
    }

    /// A stage with counters of its own, for callers that drive `dispatch_tools`
    /// without a lane behind it (tests read the channel directly).
    pub fn detached(jobs: UnboundedSender<ToolJob>) -> Self {
        Self::new(jobs, Arc::new(crate::tool_bridge::ToolLaneStats::new(1)))
    }
}

/// Context-tool results computed inline by [`dispatch_tools`] (the `context_*`
/// tools mutate the ECS window, so they can't run on the async lane), held until
/// [`collect_tools`] merges them with the lane results. Absent when a batch had
/// no context tools.
#[derive(Component, Debug, Clone, Default)]
pub struct ContextToolResults(pub Vec<(String, String)>);

/// Merge context + lane tool results into one `(id, result)` list in the
/// original tool-call order (Anthropic requires a `tool_result` per `tool_use`,
/// in order).
/// Collapse a possibly-multiline string to a single trimmed line capped at
/// `max` characters (with an ellipsis when truncated), for one-line log entries.
pub(crate) fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        format!("{}…", flat.chars().take(max).collect::<String>())
    } else {
        flat
    }
}

pub(crate) fn merge_in_call_order(
    tool_calls: &[crate::components::ToolCall],
    parts: &[(String, String)],
) -> Vec<(String, String)> {
    tool_calls
        .iter()
        .map(|tc| {
            let result = parts
                .iter()
                .find(|(id, _)| id == &tc.tool_id)
                .map(|(_, r)| r.clone())
                .unwrap_or_default();
            (tc.tool_id.clone(), result)
        })
        .collect()
}

/// Whether a tool result describes a call whose side effect never happened.
///
/// `[error]` (it ran and failed), `[denied]` (policy refused it),
/// `[unavailable]` (the stage never offered it) and `[blocked]` (the taint
/// gate stopped it) all mean the same thing to anything reasoning about what
/// the agent *did*: file tracking must not record a write that was not
/// written, and the modification counters behind a transition gate must not
/// count it as work. Separate prefix lists are exactly how a new prefix gets
/// missed - `[unavailable]` was, when dispatch began refusing unoffered tools
/// and both call sites still listed only two, and `[blocked]` was again, so a
/// taint-blocked write counted as a modification until issue #155's pass.
pub(crate) fn call_had_no_effect(result: &str) -> bool {
    result.starts_with("[error]")
        || result.starts_with("[denied]")
        || result.starts_with("[unavailable]")
        || result.starts_with("[blocked]")
}

/// The effective tool names this stage advertised, canonicalised.
///
/// The same narrowing the request builder applies: `tools`, then `tool_filter`
/// when it is set and non-empty. Deriving both from one function is what keeps
/// "what the model was offered" and "what the model may call" the same set.
pub(crate) fn offered_tool_names(stage: &StageInference) -> Vec<&str> {
    stage
        .tools
        .iter()
        .filter(|t| match stage.tool_filter.as_deref() {
            Some(filter) if !filter.is_empty() => filter.iter().any(|f| f == &t.name),
            _ => true,
        })
        .map(|t| leviath_tools::canonical_tool_name(&t.name))
        .collect()
}

/// `Some(message)` when `name` is not among the stage's advertised tools.
///
/// The message is written for the model, not the user: it says plainly that the
/// tool does not exist *here* and lists what does, so the next turn is a usable
/// call rather than a retry of the same one. A stage advertising nothing says so
/// instead of printing an empty list.
pub(crate) fn unoffered_tool_refusal(stage: &StageInference, name: &str) -> Option<String> {
    let canonical = leviath_tools::canonical_tool_name(name);
    let offered = offered_tool_names(stage);
    if offered.contains(&canonical) {
        return None;
    }
    Some(match offered.is_empty() {
        true => format!(
            "[unavailable] '{name}' is not available in this stage, which has no \
             tools at all. Answer directly instead of calling a tool."
        ),
        false => format!(
            "[unavailable] '{name}' is not available in this stage. You may call: {}.",
            offered.join(", ")
        ),
    })
}

/// How long a dispatched batch may wait for its journal record's ack before
/// running anyway. The wait is what keeps the `ToolBatch` record on disk ahead
/// of the batch's side effects; the bound is the liveness valve - a dead or
/// backed-up persistence worker degrades to an unjournaled dispatch instead of
/// wedging every tool batch behind it.
pub(crate) const BATCH_JOURNAL_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Wrap a tool-execution closure so it first waits (bounded) for the batch's
/// journal-record ack. Both outcomes - acked, or timeout/dropped sender -
/// proceed to run the batch.
pub(crate) fn barrier_then(
    exec: BoxedToolExec,
    ack: tokio::sync::oneshot::Receiver<()>,
    timeout: std::time::Duration,
) -> BoxedToolExec {
    Box::new(move || {
        Box::pin(async move {
            let _ = tokio::time::timeout(timeout, ack).await;
            exec().await
        })
    })
}

/// `Some(refusal)` when `name`'s arguments do not satisfy the schema the
/// stage advertised for it.
///
/// The def is found by canonicalising both the called name and each advertised
/// name, the same resolution `offered_tool_names` applies, so a tool offered
/// as `bash` validates a call to `shell` and vice versa. A name with no def
/// here validates as fine - after the unoffered-tool check that cannot happen,
/// and the schema's absence is not the model's mistake to be refused over.
///
/// A schema that does not compile (a typo'd Rhai `@param` type, an MCP
/// fragment this crate cannot interpret) is logged and skipped rather than
/// refused: validation must never turn a working tool into an unusable one.
///
/// Schemas are compiled per call, deliberately. They are small, calls arrive
/// at model latency, and a compiled-validator cache would need invalidating on
/// every dynamic-tools re-advertisement and scoping per agent - real
/// complexity for unmeasurable savings.
pub(crate) fn invalid_args_refusal(
    stage: &StageInference,
    name: &str,
    args: &serde_json::Value,
) -> Option<String> {
    let canonical = leviath_tools::canonical_tool_name(name);
    let tool = stage
        .tools
        .iter()
        .find(|t| leviath_tools::canonical_tool_name(&t.name) == canonical)?;
    match leviath_tools::validate_tool_args(name, &tool.parameters, args) {
        leviath_tools::ArgValidation::Valid => None,
        leviath_tools::ArgValidation::SchemaUnusable(e) => {
            tracing::warn!(
                tool = %name,
                error = %e,
                "tool schema did not compile; skipping argument validation"
            );
            None
        }
        leviath_tools::ArgValidation::Invalid(msg) => Some(msg),
    }
}

/// What `dispatch_tools` selects.
///
/// `&'static` is bevy's `WorldQuery` convention, not a claim about
/// lifetimes: the borrow is bound when the query is fetched.
type DispatchToolsQuery = (
    Entity,
    &'static AgentState,
    &'static StageInference,
    &'static crate::components::InferenceResult,
    &'static mut ContextWindow,
    Option<&'static crate::components::ToolResultRoutingComponent>,
    Option<&'static ToolSensitivities>,
    Option<&'static mut crate::taint::TaintGate>,
    Option<&'static crate::gate_prompt::GateResolved>,
    Option<&'static crate::components::GateAutoApprove>,
    Option<&'static InFlightWork>,
    Option<&'static StageCursor>,
    // Nested rather than two more members: `QueryData` is implemented up to a
    // fixed arity and this tuple had reached it. Grouping the two run-context
    // components keeps the list inside the limit without splitting the system.
    //
    // `StageProgress` is here for `runtime_info`: `AgentState.iteration` is
    // run-cumulative, so the per-stage count to compare against the stage's own
    // cap comes from there.
    (
        Option<&'static RunMetadata>,
        Option<&'static crate::pipeline::response::StageProgress>,
    ),
    Option<&'static crate::components::OutputValidators>,
    // For the submit_output guard: a submission that is exactly the name of a
    // stage in this blueprint is a routing token, not an answer.
    Option<&'static crate::pipeline::transition::AgentBlueprint>,
);

/// The resources the daemon installs, which a bare world does not have.
///
/// Every field is optional because `lev run` drives these same systems with no
/// daemon behind them: no gate lane to prompt through, no persistence lane, no
/// event sink. Bundled as one `SystemParam` so a system's signature stays about
/// what it *queries* rather than listing the six things that might be wired.
#[derive(bevy_ecs::system::SystemParam)]
pub struct DaemonServices<'w> {
    /// The taint-gate policy, when one is configured.
    pub policy: Option<Res<'w, PolicyGate>>,
    /// Rhai rules the gate consults before blocking.
    pub script_rules: Option<Res<'w, GateScriptRules>>,
    /// The hub a blocked call can prompt through.
    pub hub: Option<Res<'w, InteractionHub>>,
    /// The lane a gate prompt's answer comes back on.
    pub gate_stage: Option<Res<'w, crate::gate_prompt::GatePromptStage>>,
    /// The lane run state is written on.
    pub persist: Option<Res<'w, PersistenceStage>>,
    /// Where world events are broadcast.
    pub sink: Option<Res<'w, crate::host::WorldEventSink>>,
}

/// Tool-dispatch system: for each `ReadyForTools` agent, apply its `context_*`
/// tool calls inline (they mutate the ECS window) and hand the rest to the
/// sequential tool lane, moving it to `AwaitingTools`. If a batch is *all*
/// context tools there is nothing for the lane, so the results are applied
/// immediately and the agent loops straight back to `ReadyToInfer`. The lane
/// serializes execution, so there is no permit gate - every ready agent is
/// enqueued in turn.
///
/// A persisted agent's batch is journaled at dispatch: a `ToolBatch` record
/// (inline results pre-filled, lane calls pending) goes to the persistence lane
/// with an ack the exec waits on, and a per-call [`ToolProgress`] journals each
/// completion as a `ToolCallDone`. On a crash mid-batch, recovery replays the
/// recorded results instead of re-running their side effects (issue #96).
pub fn dispatch_tools(
    mut agents: Query<DispatchToolsQuery, With<ReadyForTools>>,
    service: Res<ToolServiceRes>,
    stage: Res<ToolStage>,
    daemon: DaemonServices,
    mut commands: Commands,
) {
    let DaemonServices {
        policy,
        script_rules,
        hub,
        gate_stage,
        persist,
        sink,
    } = daemon;
    crate::tick_scope::clear();
    let default_policy = leviath_core::PolicyConfig::default();
    let policy_ref = policy.as_ref().map(|p| &p.0).unwrap_or(&default_policy);
    let script_checker = script_rules.as_ref().map(|r| r.0.as_ref());
    // Interactive gate prompting is available only when both the hub and the
    // gate-prompt lane are wired (the daemon); otherwise blocks are returned as
    // `[blocked]` immediately, preserving the headless/non-interactive behavior.
    let interactive = hub.as_ref().zip(gate_stage.as_ref());
    for (
        entity,
        state,
        stage_inf,
        result,
        mut window,
        routing,
        sensitivities,
        mut gate,
        resolved,
        auto_gate,
        in_flight,
        cursor,
        (metadata, stage_progress),
        validators,
        blueprint,
    ) in agents.iter_mut()
    {
        crate::tick_scope::enter(entity);
        // `--yolo`: waive taint-gate enforcement so a headless run never blocks
        // on a gate prompt no one can answer (taint tracking still records).
        let auto_approve_gates = auto_gate.is_some();
        if state.status != AgentStatus::Active {
            continue; // paused / waiting / cancelled - don't start new work
        }

        // Apply context_* tools inline (they need world access); collect the rest
        // for the async lane. A taint-gated agent's outbound call that would leak
        // over-cleared data (and isn't allowlisted) is blocked - either returned
        // as `[blocked]`, or (interactive) held for a user gate prompt.
        let mut context_results = Vec::new();
        let mut lane_calls = Vec::new();
        // A final output submitted in this batch, committed to the entity after
        // the loop (the loop holds borrows `commands` would conflict with).
        let mut submitted: Option<leviath_core::output::FinalOutput> = None;
        // A `fan_out` call in this batch, started after the loop for the same
        // reason: it parks the agent, which is a `commands` write.
        let mut fan_out: Option<(String, crate::fanout::FanOutRequest)> = None;
        // (tool_id, name, taint, clearance) for blocked calls awaiting a prompt.
        let mut pending_prompts: Vec<(
            String,
            String,
            leviath_core::TaintLevel,
            leviath_core::TaintLevel,
        )> = Vec::new();
        for c in &result.tool_calls {
            // Layer 1, enforced rather than merely advertised.
            //
            // A stage's `available_tools` was applied only when building the
            // schema list sent to the model. Nothing checked it again here, so a
            // model that *named* a tool it had never been offered got that call
            // dispatched anyway - reaching the permission gate, and for a
            // default-`Ask` tool surfacing to the user as an approval prompt for
            // something the stage was never granted.
            //
            // That is not hypothetical. A `plan` stage granting only
            // `read_file`/`list_dir`/`ask_user_*`/`edit_document` emitted
            // `write_file` with a complete source file in it, and the user was
            // asked to approve writing code from the planning stage. Declining
            // it was the only thing that stopped it.
            //
            // Checked against `StageInference`, which *is* the set advertised
            // for this stage - resolved at spawn, swapped on every transition,
            // and rewritten by the dynamic-tools refresh - so enforcement cannot
            // drift from advertising the way a second copy of the rule would.
            if let Some(refusal) = unoffered_tool_refusal(stage_inf, &c.name) {
                context_results.push((c.tool_id.clone(), refusal));
                continue;
            }
            // Layer 2: the call must satisfy the schema the model was shown.
            // A mismatched call is refused back to the model with the
            // validator's message, so the next turn can self-correct, rather
            // than executed on garbage or surfaced to the user as a permission
            // prompt for arguments that were never valid. Deterministic, so a
            // gate-prompt re-run of the same batch refuses identically.
            if let Some(refusal) = invalid_args_refusal(stage_inf, &c.name, &c.arguments) {
                context_results.push((c.tool_id.clone(), refusal));
                continue;
            }
            // Answered inline for the same reason the context tools are: the
            // stage, the iteration counts and the window occupancy it reports
            // live in the world, which the async lane cannot reach.
            if crate::runtime_info_tool::is_runtime_info_tool(&c.name) {
                let stage_max = blueprint
                    .zip(cursor)
                    .and_then(|(bp, cur)| bp.0.stages.get(cur.index))
                    .and_then(|s| s.max_iterations);
                let facts = crate::runtime_info_tool::RuntimeFacts {
                    version: env!("CARGO_PKG_VERSION"),
                    run_id: metadata.map(|m| m.run_id.as_str()),
                    agent: metadata.map(|m| m.agent_name.as_str()),
                    stage: &state.current_stage,
                    stage_index: cursor
                        .zip(metadata)
                        .map(|(cur, m)| (cur.index, m.num_stages)),
                    stage_iterations: (
                        stage_progress.map(|p| p.iterations).unwrap_or(0),
                        stage_max,
                    ),
                    total_iterations: state.iteration,
                    provider_model: (&stage_inf.provider_name, &stage_inf.model),
                    tools: stage_inf.tools.iter().map(|t| t.name.as_str()).collect(),
                    unattended: metadata.is_some_and(|m| m.unattended),
                    workdir: metadata.map(|m| m.workdir.as_str()),
                };
                let text = crate::runtime_info_tool::handle_runtime_info(&facts, &window);
                context_results.push((c.tool_id.clone(), text));
                continue;
            }
            if crate::context_tools::is_context_tool(&c.name) {
                let text =
                    crate::context_tools::handle_context_tool(&c.name, &c.arguments, &mut window);
                context_results.push((c.tool_id.clone(), text));
                continue;
            }
            // Applied inline for the same reason the context tools are: it
            // writes the live window and an ECS component, neither of which the
            // async lane can reach. Recorded here and committed after the loop,
            // because `commands` cannot be borrowed inside it.
            if crate::output_tool::is_output_tool(&c.name) {
                let stage_names: Vec<String> = blueprint
                    .map(|bp| bp.0.stages.iter().map(|s| s.name.clone()).collect())
                    .unwrap_or_default();
                let (text, output) = crate::output_tool::handle_output_tool(
                    &c.arguments,
                    &crate::output_tool::OutputContext {
                        spec: stage_inf.output.as_ref(),
                        validators,
                        stage: &state.current_stage,
                        stage_names: &stage_names,
                        workdir: metadata.map(|m| std::path::Path::new(&m.workdir)),
                    },
                    chrono::Utc::now().timestamp(),
                    &mut window,
                );
                // A refused submission leaves any earlier one alone: a bad
                // correction must not erase a good answer.
                if let Some(output) = output {
                    submitted = Some(output);
                }
                context_results.push((c.tool_id.clone(), text));
                continue;
            }
            // Read inline, started after the loop. Like `submit_output` it needs
            // world access the async lane does not have - it parks this agent on
            // its workers - and like the context tools it is applied here rather
            // than dispatched.
            if crate::fanout::is_fan_out_tool(&c.name) {
                let text = match crate::fanout::parse_fan_out_call(&c.arguments) {
                    // One per batch. A second would need a second parked state
                    // on one agent, and there is no work it could do that adding
                    // its items to the first call would not: the engine paces
                    // the concurrency either way.
                    Ok(_) if fan_out.is_some() => Some(format!(
                        "[error] only one {} call per turn - put all the work in \
                         one call, the concurrency is paced for you",
                        leviath_core::blueprint::FAN_OUT_TOOL
                    )),
                    Ok(request) => {
                        fan_out = Some((c.tool_id.clone(), request));
                        None
                    }
                    Err(e) => Some(format!("[error] {e}")),
                };
                if let Some(text) = text {
                    context_results.push((c.tool_id.clone(), text));
                }
                continue;
            }
            // A call the user already resolved in a prior prompt round.
            if let Some(resolved) = resolved {
                if let Some(msg) = resolved.denied.get(&c.tool_id) {
                    context_results.push((c.tool_id.clone(), msg.clone()));
                    continue;
                }
                if resolved.approved.contains(&c.tool_id) {
                    lane_calls.push(leviath_providers::ToolCall {
                        id: c.tool_id.clone(),
                        name: c.name.clone(),
                        arguments: c.arguments.clone(),
                        thought_signature: c.thought_signature.clone(),
                    });
                    continue;
                }
            }
            if let Some(gate) = gate.as_deref_mut() {
                let decision = gate.check_with_policy(
                    &state.agent_id,
                    &c.name,
                    &window,
                    None,
                    policy_ref,
                    script_checker,
                );
                if !decision.is_allowed() {
                    if auto_approve_gates {
                        // `--yolo`: waive enforcement but record the override in
                        // the audit trail (rather than skipping the gate), so the
                        // over-cleared call is still accounted for. Fall through
                        // to dispatch the call.
                        let (taint, clearance) = decision
                            .blocked_levels()
                            .expect("a non-Allowed GateDecision is always Blocked");
                        gate.record_allow(
                            &state.agent_id,
                            &c.name,
                            taint,
                            clearance,
                            leviath_core::taint::GateDecisionSource::YoloAutoApprove,
                        );
                    } else {
                        match (interactive, decision.blocked_levels()) {
                            (Some(_), Some((taint, clearance))) => {
                                pending_prompts.push((
                                    c.tool_id.clone(),
                                    c.name.clone(),
                                    taint,
                                    clearance,
                                ));
                            }
                            _ => {
                                context_results
                                    .push((c.tool_id.clone(), taint_block_message(&decision)));
                            }
                        }
                        continue;
                    }
                }
            }
            lane_calls.push(leviath_providers::ToolCall {
                id: c.tool_id.clone(),
                name: c.name.clone(),
                arguments: c.arguments.clone(),
                thought_signature: c.thought_signature.clone(),
            });
        }

        // Commit a submitted output before any of the paths below can take an
        // early exit, so an answer is recorded whether the rest of the batch
        // dispatches, holds for a gate prompt, or turns out to be empty.
        // Re-applying it on a gate-prompt re-run is harmless: the same
        // submission produces the same component.
        if let Some(output) = submitted {
            commands
                .entity(entity)
                .insert(crate::persistence::FinalOutput(output));
        }

        // Hold the batch and ask the user about each blocked call.
        if let (false, Some((hub, gate_stage))) = (pending_prompts.is_empty(), interactive) {
            let n = pending_prompts.len();
            for (tool_id, name, taint, clearance) in pending_prompts {
                gate_stage
                    .runtime
                    .spawn(crate::gate_prompt::run_gate_prompt(
                        crate::gate_prompt::GatedCall {
                            entity,
                            agent_id: state.agent_id.clone(),
                            tool_id,
                            tool_name: name,
                            taint,
                            clearance,
                        },
                        crate::interaction_hub::PromptLane {
                            hub: (*hub).clone(),
                            outcomes: gate_stage.outcomes.clone(),
                            wake: gate_stage.wake.clone(),
                        },
                    ));
            }
            commands
                .entity(entity)
                .remove::<ReadyForTools>()
                .insert(crate::gate_prompt::AwaitingGatePrompt(n))
                .insert(crate::gate_prompt::GateResolved::default());
            continue; // re-run after the prompts resolve
        }

        // Dispatching the batch consumes any resolution state from a prior round.
        commands
            .entity(entity)
            .remove::<crate::gate_prompt::GateResolved>();

        // A fan-out parks this agent, so it cannot share a batch with lane calls
        // that would still be running when it does. Refused rather than
        // serialized: "call it on its own" is a rule a model can follow, and a
        // half-dispatched batch is not something it could reason about.
        if let Some((call_id, _)) = &fan_out
            && !lane_calls.is_empty()
        {
            context_results.push((
                call_id.clone(),
                format!(
                    "[error] {} has to be the only tool call in its turn, because it \
                     waits for its workers. Call it on its own.",
                    leviath_core::blueprint::FAN_OUT_TOOL
                ),
            ));
            fan_out = None;
        }
        if let Some((call_id, request)) = fan_out {
            // Everything else in the batch lands now; the fan-out's own result
            // arrives when its workers finish, as that call's tool result.
            //
            // It must be dropped from the results applied here, and only from
            // those: the call stays in `tool_calls` so the assistant turn keeps
            // its `tool_use` block, but `merge_in_call_order` fills a call with
            // no entry in `context_results` with an empty string, and that
            // placeholder plus the real report from `finish_tool_fan_out` is
            // two `tool_result` blocks under one id. Anthropic rejects the next
            // request outright: "each tool_use must have a single result".
            // Deferring is safe because the agent parks on its workers, so no
            // request goes out carrying a `tool_use` that has no result yet.
            let merged: Vec<(String, String)> =
                merge_in_call_order(&result.tool_calls, &context_results)
                    .into_iter()
                    .filter(|(id, _)| id != &call_id)
                    .collect();
            apply_tool_results(
                &mut window,
                &result.response,
                &result.tool_calls,
                &merged,
                routing.map(|c| &c.routing),
                sensitivities.map(|s| &s.0),
            );
            commands
                .entity(entity)
                .remove::<ReadyForTools>()
                .insert(crate::fanout::PendingFanOut { call_id, request });
            continue;
        }

        if lane_calls.is_empty() {
            // Nothing async to run - apply the context results now and loop back.
            let merged = merge_in_call_order(&result.tool_calls, &context_results);
            apply_tool_results(
                &mut window,
                &result.response,
                &result.tool_calls,
                &merged,
                routing.map(|c| &c.routing),
                sensitivities.map(|s| &s.0),
            );
            commands
                .entity(entity)
                .remove::<ReadyForTools>()
                .insert(ReadyToInfer);
            continue;
        }

        // Journal the batch before it can run: a `ToolBatch` record with the
        // dispatcher's inline results pre-filled and every lane call pending,
        // plus a per-call progress hook that records each completion. Worlds
        // without a persistence lane or run metadata (tests, unpersisted
        // agents) dispatch unjournaled with a no-op progress.
        let (progress, ack) = match (persist.as_ref(), metadata) {
            (Some(persist), Some(md)) => {
                let record = leviath_core::run_archive::RunRecord::ToolBatch {
                    calls: result
                        .tool_calls
                        .iter()
                        .map(|c| leviath_core::run_archive::ToolCallRecord {
                            id: c.tool_id.clone(),
                            name: c.name.clone(),
                            arguments: c.arguments.to_string(),
                            result: context_results
                                .iter()
                                .find(|(id, _)| id == &c.tool_id)
                                .map(|(_, r)| r.clone()),
                            thought_signature: c.thought_signature.clone(),
                        })
                        .collect(),
                    at: chrono::Utc::now().timestamp(),
                    stage_index: cursor.map_or(0, |c| c.index),
                    iteration: state.iteration,
                    response: result.response.clone(),
                };
                let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
                let _ = persist.0.send(PersistMsg::Append {
                    run_id: md.run_id.clone(),
                    record: Box::new(record),
                    ack: Some(ack_tx),
                });
                let sender = persist.0.clone();
                let run_id = md.run_id.clone();
                let iteration = state.iteration;
                let progress: ToolProgress = Arc::new(move |call_id: &str, result: &str| {
                    let _ = sender.send(PersistMsg::Append {
                        run_id: run_id.clone(),
                        record: Box::new(leviath_core::run_archive::RunRecord::ToolCallDone {
                            iteration,
                            call_id: call_id.to_string(),
                            result: result.to_string(),
                            at: chrono::Utc::now().timestamp(),
                        }),
                        ack: None,
                    });
                });
                (progress, Some(ack_rx))
            }
            _ => (noop_progress(), None),
        };
        // Announce each lane-bound call before it starts executing. Inline
        // results (context tools, refusals, blocks) never reach the lane and
        // are deliberately not announced.
        if let (Some(sink), Some(md)) = (sink.as_ref(), metadata) {
            for call in &lane_calls {
                let _ = sink.0.send(crate::host::WorldEvent::ToolCallStarted {
                    run_id: md.run_id.clone(),
                    agent_id: state.agent_id.clone(),
                    call_id: call.id.clone(),
                    tool: call.name.clone(),
                });
            }
        }
        let exec = service.0.exec_for(entity, lane_calls, progress);
        let exec = match ack {
            Some(ack) => barrier_then(exec, ack, BATCH_JOURNAL_ACK_TIMEOUT),
            None => exec,
        };
        let cancel = crate::cancel::CancelToken::new();
        // The lane is alive for the world's lifetime; a failed send would
        // only happen during shutdown, where dropping the job is fine.
        stage.stats.enqueued();
        let _ = stage.jobs.send(ToolJob {
            entity,
            exec,
            cancel: cancel.clone(),
        });
        track_in_flight(&mut commands, entity, in_flight, cancel);
        commands
            .entity(entity)
            .remove::<ReadyForTools>()
            .insert(AwaitingTools)
            .insert(ContextToolResults(context_results));
    }
}
