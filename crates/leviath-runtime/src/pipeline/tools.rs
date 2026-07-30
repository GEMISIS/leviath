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

/// Provides a per-agent tool-execution closure. The concrete implementation
/// (in the CLI) holds each agent's tool registry, workdir, and permission
/// policy; the pipeline stays agnostic to *how* tools run. `exec_for` returns a
/// boxed closure the tool worker runs off the tick.
pub trait ToolService: Send + Sync {
    /// Build the closure that runs `calls` for `entity`, resolving `(id, result)`
    /// pairs.
    fn exec_for(&self, entity: Entity, calls: Vec<leviath_providers::ToolCall>) -> BoxedToolExec;

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

/// The job sender feeding the tool lane, as a world resource.
#[derive(Resource, Clone)]
pub struct ToolStage(pub UnboundedSender<ToolJob>);

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
/// `[error]` (it ran and failed), `[denied]` (policy refused it) and
/// `[unavailable]` (the stage never offered it) all mean the same thing to
/// anything reasoning about what the agent *did*: file tracking must not record
/// a write that was not written, and the modification counters behind a
/// transition gate must not count it as work. Three separate prefix lists is
/// exactly how a fourth prefix gets missed - `[unavailable]` was, when dispatch
/// began refusing unoffered tools and both call sites still listed only two.
pub(crate) fn call_had_no_effect(result: &str) -> bool {
    result.starts_with("[error]")
        || result.starts_with("[denied]")
        || result.starts_with("[unavailable]")
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

/// Tool-dispatch system: for each `ReadyForTools` agent, apply its `context_*`
/// tool calls inline (they mutate the ECS window) and hand the rest to the
/// sequential tool lane, moving it to `AwaitingTools`. If a batch is *all*
/// context tools there is nothing for the lane, so the results are applied
/// immediately and the agent loops straight back to `ReadyToInfer`. The lane
/// serializes execution, so there is no permit gate - every ready agent is
/// enqueued in turn.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub fn dispatch_tools(
    mut agents: Query<
        (
            Entity,
            &AgentState,
            &StageInference,
            &crate::components::InferenceResult,
            &mut ContextWindow,
            Option<&crate::components::ToolResultRoutingComponent>,
            Option<&ToolSensitivities>,
            Option<&mut crate::taint::TaintGate>,
            Option<&crate::gate_prompt::GateResolved>,
            Option<&crate::components::GateAutoApprove>,
            Option<&InFlightWork>,
        ),
        With<ReadyForTools>,
    >,
    service: Res<ToolServiceRes>,
    stage: Res<ToolStage>,
    policy: Option<Res<PolicyGate>>,
    script_rules: Option<Res<GateScriptRules>>,
    hub: Option<Res<InteractionHub>>,
    gate_stage: Option<Res<crate::gate_prompt::GatePromptStage>>,
    mut commands: Commands,
) {
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
            if crate::context_tools::is_context_tool(&c.name) {
                let text =
                    crate::context_tools::handle_context_tool(&c.name, &c.arguments, &mut window);
                context_results.push((c.tool_id.clone(), text));
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

        // Hold the batch and ask the user about each blocked call.
        if let (false, Some((hub, gate_stage))) = (pending_prompts.is_empty(), interactive) {
            let n = pending_prompts.len();
            for (tool_id, name, taint, clearance) in pending_prompts {
                gate_stage
                    .runtime
                    .spawn(crate::gate_prompt::run_gate_prompt(
                        entity,
                        (*hub).clone(),
                        state.agent_id.clone(),
                        tool_id,
                        name,
                        taint,
                        clearance,
                        gate_stage.outcomes.clone(),
                        gate_stage.wake.clone(),
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

        let exec = service.0.exec_for(entity, lane_calls);
        let cancel = crate::cancel::CancelToken::new();
        // The lane worker is alive for the world's lifetime; a failed send would
        // only happen during shutdown, where dropping the job is fine.
        let _ = stage.0.send(ToolJob {
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
