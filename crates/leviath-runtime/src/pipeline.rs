//! The ECS pipeline (Phase 2): components + systems that drive every agent
//! through check-input → infer → tools → apply → repeat, entirely as data.
//!
//! Agents are entities; their execution phase is a **marker component**
//! (`ReadyToInfer`, `AwaitingInference`, …) so systems can query by phase. A
//! system never blocks on I/O: the dispatch systems hand work to the async
//! bridges ([`crate::inference_bridge`], [`crate::tool_bridge`]) and the collect
//! systems apply the results on a later tick. This module is built alongside the
//! existing imperative engine; the two are unified in a later phase.

use std::sync::Arc;

use bevy_ecs::prelude::*;
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use leviath_providers::{InferenceRequest, Provider, Tool};

use crate::compaction_bridge::{CompactionJob, CompactionOutcome, run_compaction_job};
use crate::components::{
    AgentMessage, AgentState, AgentStatus, AwaitingInteraction, ContextWindow, InferenceConfig,
    MessageInbox,
};
use crate::fanout::FanOutWaiting;
use crate::inference_bridge::{InferenceJob, InferenceOutcome, run_inference_job};
use crate::inference_pool::InferencePools;
use crate::interaction_hub::InteractionHub;
use crate::persistence::{RunMetadata, TokenTotals, build_context_snapshot, build_run_meta};
use crate::persistence_bridge::PersistJob;
use crate::providers::ProviderRegistry;
use crate::tool_bridge::{BoxedToolExec, ToolJob, ToolOutcome};

// ─── Phase marker components (an agent is in exactly one) ────────────────────

/// The agent is active and ready to build a request and (permits allowing)
/// dispatch inference.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyToInfer;

/// Inference has been dispatched to the pool; the agent is waiting for its
/// result (which the inference-collect system will apply).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AwaitingInference;

/// Transient tag: the agent just entered a stage (index + name). The
/// [`sync_tool_stages`] system reads it to notify the [`ToolService`] of the
/// stage change, then removes it. Carries the data so the tool service need not
/// query the world.
#[derive(Component, Debug, Clone)]
pub struct StageJustEntered {
    /// The new stage's index.
    pub index: usize,
    /// The new stage's name.
    pub name: String,
}

// ─── Per-agent stage data the dispatch system reads ──────────────────────────

/// Resolved inference parameters for the agent's current stage, set when it
/// enters that stage. Pure data — the dispatch system reads it to build the
/// request.
#[derive(Component, Debug, Clone)]
pub struct StageInference {
    /// Registered provider to call.
    pub provider_name: String,
    /// Model id (also the key into the per-model inference pools).
    pub model: String,
    /// Tools advertised at this stage.
    pub tools: Vec<Tool>,
    /// Optional allow-list of tool names (`None`/empty = all `tools`).
    pub tool_filter: Option<Vec<String>>,
}

// ─── World resources for the inference stage ─────────────────────────────────

/// The registered providers, as a world resource.
#[derive(Resource)]
pub struct Providers(pub ProviderRegistry);

/// The plumbing the inference-dispatch system needs: the per-model pools, the
/// channel to report outcomes on, the tick wake handle, and a runtime handle to
/// spawn the (bounded, per-request) worker tasks onto.
#[derive(Resource, Clone)]
pub struct InferenceStage {
    /// Per-model concurrency pools.
    pub pools: Arc<InferencePools>,
    /// Where completed inferences are reported.
    pub outcomes: UnboundedSender<InferenceOutcome>,
    /// Where completed *transition-choice* inferences are reported (a separate
    /// lane so the collect systems don't confuse a routing decision with a normal
    /// agent turn).
    pub transition_outcomes: UnboundedSender<InferenceOutcome>,
    /// Where completed *compaction* jobs (LLM context summarization) are
    /// reported — again a separate lane so a summary isn't mistaken for a turn.
    pub compaction_outcomes: UnboundedSender<crate::compaction_bridge::CompactionOutcome>,
    /// Signalled when an inference completes, to wake the tick loop.
    pub wake: Arc<Notify>,
    /// Runtime the worker tasks are spawned onto.
    pub runtime: Handle,
}

/// Truncate `text` to at most `max_chars` characters, never splitting a
/// multi-byte UTF-8 char. `max_chars` is an approximate char budget the caller
/// derives from a token estimate.
fn truncate_on_char_boundary(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Build the [`InferenceRequest`] for an agent from its context window + stage
/// data. Pure; no `.await`. (Ported from `AgentEngine::build_inference_request`,
/// with provider resolution lifted into the caller so this stays query-friendly.)
fn build_request(
    window: &ContextWindow,
    config: Option<&InferenceConfig>,
    stage: &StageInference,
    provider: &Arc<dyn Provider>,
) -> InferenceRequest {
    let assembled = window.assemble();
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

    InferenceRequest {
        system: assembled.system_blocks,
        messages: assembled.messages,
        model: stage.model.clone(),
        max_tokens,
        temperature,
        tools: filtered_tools,
        extra: serde_json::Value::Null,
    }
}

/// Inference-dispatch system: for every `ReadyToInfer` agent, resolve its
/// provider and, **if a per-model permit is free**, build the request, spawn the
/// inference job, and move it to `AwaitingInference`. If its provider is missing
/// or no slot is free, it stays `ReadyToInfer` and is retried on a later tick —
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
        ),
        With<ReadyToInfer>,
    >,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    par_commands: ParallelCommands,
) {
    // Fan out across ready agents: request assembly (`build_request`) is the
    // per-agent CPU cost and is independent, so it runs in parallel on the
    // compute pool. Permit acquisition (an atomic semaphore) and the tokio spawn
    // are thread-safe; the marker swap is batched via `ParallelCommands`.
    agents
        .par_iter()
        .for_each(|(entity, state, window, config, si)| {
            if state.status != AgentStatus::Active {
                return; // paused / waiting / cancelled — don't start new work
            }
            let Some(provider) = providers.0.get(&si.provider_name).cloned() else {
                return; // provider not registered — leave ready, retry later
            };
            let Some(permit) = stage.pools.try_acquire(&si.model) else {
                return; // pool full — leave ready, retry next tick
            };
            let request = build_request(window, config, si, &provider);
            let job = InferenceJob {
                entity,
                provider,
                request,
                permit,
            };
            stage.runtime.spawn(run_inference_job(
                job,
                stage.outcomes.clone(),
                stage.wake.clone(),
            ));
            par_commands.command_scope(|mut commands| {
                commands
                    .entity(entity)
                    .remove::<ReadyToInfer>()
                    .insert(AwaitingInference);
            });
        });
}

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
fn to_inference_result(
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
        ),
        With<AwaitingInference>,
    >,
    mut commands: Commands,
) {
    while let Ok(outcome) = results.0.try_recv() {
        let Ok((mut state, totals, cursor, mut ledger, buffer)) = agents.get_mut(outcome.entity)
        else {
            continue; // stale: agent cancelled/despawned since dispatch
        };
        let idx = cursor.map_or(0, |c| c.index);
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
}

/// How a stage ended, when that governs the transition. Absent ⇒ the stage
/// completed normally. Read by [`resolve_transition`] to follow an
/// `error`/`max_iterations`-conditioned edge (e.g. → error_recovery) when the
/// stage errored or hit its iteration cap.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub enum StageOutcome {
    /// The stage errored (carries the error message for the terminal case).
    Errored(String),
    /// The stage hit its `max_iterations` cap.
    MaxIterations,
}

/// One [`StageRecord`](leviath_core::run_meta::StageRecord) per blueprint stage,
/// seeded at spawn (names + `Pending`) and reconciled by [`dispatch_persistence`]
/// (status + timestamps), with per-stage tokens accrued by [`collect_inference`].
/// Serialized to `stages.json` so the dashboard / serve API can show every
/// stage's real name and status — not just the active one (whose name is the only
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
/// routing — no I/O.
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
    for (entity, result, mut progress, totals) in agents.iter_mut() {
        progress.iterations += 1; // per-stage inference count (for max_iterations)
        let mut e = commands.entity(entity);
        e.remove::<ProcessResponse>();
        if result.tool_calls.is_empty() {
            e.insert(ReadyForTransition);
        } else {
            progress.total_tool_calls += result.tool_calls.len();
            if let Some(mut totals) = totals {
                totals.tool_calls += result.tool_calls.len();
            }
            e.insert(ReadyForTools);
        }
    }
}

/// The "use your tools" nudge injected when a model responds with text before
/// making any tool call.
const NUDGE_TEXT: &str = "You have tools available. Please use them to complete the task. Start by reading the relevant files in the working directory.";
/// How many text-only responses to nudge before accepting the text as final.
const MAX_TEXT_ONLY_NUDGES: usize = 3;

/// Empty-response system: for each `ReadyForTransition` agent decide whether the
/// stage is done. If the agent has already made tool calls, or we've nudged the
/// max number of times, the text response is accepted and the agent advances to
/// `ResolveTransition`. Otherwise (text only, no work yet) the response + a
/// "use your tools" nudge are added to context and the agent loops back to
/// `ReadyToInfer`. Ported from `AgentEngine::loop_handle_empty_tool_calls`.
pub fn handle_empty_response(
    mut agents: Query<
        (
            Entity,
            &mut ContextWindow,
            &crate::components::InferenceResult,
            &mut StageProgress,
        ),
        With<ReadyForTransition>,
    >,
    mut commands: Commands,
) {
    for (entity, mut window, infer, mut progress) in agents.iter_mut() {
        if progress.total_tool_calls > 0 || progress.text_only_nudges >= MAX_TEXT_ONLY_NUDGES {
            commands
                .entity(entity)
                .remove::<ReadyForTransition>()
                .insert(ResolveTransition);
        } else {
            progress.text_only_nudges += 1;
            let response_tokens = infer.response.len() / 4 + 1;
            let _ = window.add_typed_entry(
                "conversation",
                leviath_core::EntryKind::AssistantTurn { tool_calls: vec![] },
                infer.response.clone(),
                response_tokens,
            );
            let nudge_tokens = NUDGE_TEXT.len() / 4 + 1;
            let _ = window.add_typed_entry(
                "conversation",
                leviath_core::EntryKind::UserMessage,
                NUDGE_TEXT.to_string(),
                nudge_tokens,
            );
            commands
                .entity(entity)
                .remove::<ReadyForTransition>()
                .insert(ReadyToInfer);
        }
    }
}

/// The agent's tool batch has been handed to the tool lane; it is waiting for
/// the results (which the tool-collect system will apply).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AwaitingTools;

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
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        format!("{}…", flat.chars().take(max).collect::<String>())
    } else {
        flat
    }
}

fn merge_in_call_order(
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

/// Tool-dispatch system: for each `ReadyForTools` agent, apply its `context_*`
/// tool calls inline (they mutate the ECS window) and hand the rest to the
/// sequential tool lane, moving it to `AwaitingTools`. If a batch is *all*
/// context tools there is nothing for the lane, so the results are applied
/// immediately and the agent loops straight back to `ReadyToInfer`. The lane
/// serializes execution, so there is no permit gate — every ready agent is
/// enqueued in turn.
#[allow(clippy::type_complexity)]
pub fn dispatch_tools(
    mut agents: Query<
        (
            Entity,
            &AgentState,
            &crate::components::InferenceResult,
            &mut ContextWindow,
            Option<&crate::components::ToolResultRoutingComponent>,
            Option<&ToolSensitivities>,
            Option<&mut crate::taint::TaintGate>,
        ),
        With<ReadyForTools>,
    >,
    service: Res<ToolServiceRes>,
    stage: Res<ToolStage>,
    policy: Option<Res<PolicyGate>>,
    script_rules: Option<Res<GateScriptRules>>,
    mut commands: Commands,
) {
    let default_policy = leviath_core::PolicyConfig::default();
    let policy_ref = policy.as_ref().map(|p| &p.0).unwrap_or(&default_policy);
    let script_checker = script_rules.as_ref().map(|r| r.0.as_ref());
    for (entity, state, result, mut window, routing, sensitivities, mut gate) in agents.iter_mut() {
        if state.status != AgentStatus::Active {
            continue; // paused / waiting / cancelled — don't start new work
        }

        // Apply context_* tools inline (they need world access); collect the rest
        // for the async lane. A taint-gated agent's outbound calls that would leak
        // over-cleared data (and aren't allowlisted) are blocked here with a
        // `[blocked]` result instead of reaching the executor.
        let mut context_results = Vec::new();
        let mut lane_calls = Vec::new();
        for c in &result.tool_calls {
            if crate::context_tools::is_context_tool(&c.name) {
                let text =
                    crate::context_tools::handle_context_tool(&c.name, &c.arguments, &mut window);
                context_results.push((c.tool_id.clone(), text));
                continue;
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
                    context_results.push((c.tool_id.clone(), taint_block_message(&decision)));
                    continue;
                }
            }
            lane_calls.push(leviath_providers::ToolCall {
                id: c.tool_id.clone(),
                name: c.name.clone(),
                arguments: c.arguments.clone(),
            });
        }

        if lane_calls.is_empty() {
            // Nothing async to run — apply the context results now and loop back.
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
        // The lane worker is alive for the world's lifetime; a failed send would
        // only happen during shutdown, where dropping the job is fine.
        let _ = stage.0.send(ToolJob { entity, exec });
        commands
            .entity(entity)
            .remove::<ReadyForTools>()
            .insert(AwaitingTools)
            .insert(ContextToolResults(context_results));
    }
}

/// Per-tool output sensitivity for an agent, populated by the taint-gate system
/// when taint tracking is enabled. Absent ⇒ taint off ⇒ results tagged Public.
#[derive(Component, Debug, Clone, Default)]
pub struct ToolSensitivities(pub std::collections::HashMap<String, leviath_core::TaintLevel>);

/// The tool allowlist policy (`policy.toml`), as a world resource. The daemon
/// inserts it; a taint-gated agent's outbound calls are checked against it. When
/// absent, the gate falls back to an empty policy (deny-by-clearance only).
#[derive(Resource, Default)]
pub struct PolicyGate(pub leviath_core::PolicyConfig);

/// The scripted gate rules (`~/.config/leviath/rules/*.rhai`), as a world
/// resource. The daemon builds the checker (it owns the Rhai engine); the gate
/// consults it after the static allowlist. Absent ⇒ no scripted rules.
#[derive(Resource, Clone)]
pub struct GateScriptRules(pub std::sync::Arc<crate::taint::ScriptRuleChecker>);

/// The `[blocked]` tool result produced when the taint gate denies an outbound
/// call: enough for the model to understand why and adjust.
fn taint_block_message(decision: &leviath_core::taint::GateDecision) -> String {
    match decision {
        leviath_core::taint::GateDecision::Blocked {
            taint_level,
            clearance,
            tool_name,
            source_regions,
        } => format!(
            "[blocked] Tool '{tool_name}' would send {taint_level:?}-level data over a channel \
             cleared only for {clearance:?} (tainted by: {}). Add an allowlist rule with \
             `lev policy add` to permit it.",
            if source_regions.is_empty() {
                "context".to_string()
            } else {
                source_regions.join(", ")
            }
        ),
        // Only ever called on a Blocked decision.
        leviath_core::taint::GateDecision::Allowed => "[blocked] tool call denied".to_string(),
    }
}

/// The receiving end of the tool-outcomes channel, as a world resource.
#[derive(Resource)]
pub struct ToolResults(pub UnboundedReceiver<ToolOutcome>);

/// Apply a completed tool batch to an agent's context window: add the assistant
/// turn (with its tool calls) then each tool result, honoring the stage's
/// tool-result routing (target region, `persist=false`→scratch, per-result
/// truncation) and, when a per-tool sensitivity is provided, tagging the result
/// with that taint level. Tool results MUST be added (Anthropic requires a
/// `tool_result` for every `tool_use`), so an over-budget region truncates or
/// falls back to a placeholder rather than dropping. Ported from the core of
/// `AgentEngine::loop_apply_tool_results` (repetition + message draining are
/// separate systems).
fn apply_tool_results(
    window: &mut ContextWindow,
    response_content: &str,
    tool_calls: &[crate::components::ToolCall],
    tool_results: &[(String, String)],
    routing: Option<&leviath_core::blueprint::ToolResultRouting>,
    sensitivities: Option<&std::collections::HashMap<String, leviath_core::TaintLevel>>,
) {
    let response_tokens = response_content.len() / 4;
    let serialized: Vec<leviath_core::SerializedToolCall> = tool_calls
        .iter()
        .map(|tc| leviath_core::SerializedToolCall {
            id: tc.tool_id.clone(),
            name: tc.name.clone(),
            arguments: tc.arguments.clone(),
        })
        .collect();
    let _ = window.add_typed_entry(
        "conversation",
        leviath_core::EntryKind::AssistantTurn {
            tool_calls: serialized,
        },
        response_content.to_string(),
        response_tokens,
    );

    for (tool_call_id, result) in tool_results {
        let mut result_text = result.clone();
        let tool_name = tool_calls
            .iter()
            .find(|tc| tc.tool_id == *tool_call_id)
            .map(|tc| tc.name.clone())
            .unwrap_or_default();

        if let Some(routing) = routing
            && let Some(max_tokens) = routing.max_result_tokens
        {
            let max_chars = max_tokens * 4;
            if result_text.len() > max_chars {
                result_text = truncate_on_char_boundary(&result_text, max_chars);
                result_text.push_str("\n[...truncated]");
            }
        }
        let result_tokens = result_text.len() / 4 + 1;

        let base_region = match routing {
            Some(r) => r
                .tool_overrides
                .get(&tool_name)
                .map(String::as_str)
                .unwrap_or(r.default_region.as_str()),
            None => "conversation",
        };
        let target_region = match routing {
            Some(r) if !r.persist && window.get_region("scratch").is_some() => "scratch",
            _ => base_region,
        };

        let taint_level = sensitivities.map(|s| {
            s.get(&tool_name)
                .copied()
                .unwrap_or(leviath_core::TaintLevel::Public)
        });
        let add = |window: &mut ContextWindow, region: &str, content: String, tokens: usize| {
            let kind = leviath_core::EntryKind::ToolResult {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                is_error: false,
            };
            match taint_level {
                Some(level) => {
                    window.add_typed_tainted_to_region(region, kind, content, tokens, level)
                }
                None => window.add_typed_entry(region, kind, content, tokens),
            }
        };

        if add(window, target_region, result_text.clone(), result_tokens).is_err() {
            let available = window
                .get_region(target_region)
                .map(|r| r.max_tokens.saturating_sub(r.current_tokens))
                .unwrap_or(0);
            let truncated = if available > 100 {
                let char_budget = (available - 10) * 4;
                let prefix = truncate_on_char_boundary(&result_text, char_budget);
                let omitted = result_text.len().saturating_sub(prefix.len());
                format!("{}... [truncated, {} chars omitted]", prefix, omitted)
            } else {
                "[tool result truncated — context window full]".to_string()
            };
            let trunc_tokens = truncated.len() / 4 + 1;
            if add(window, target_region, truncated, trunc_tokens).is_err() {
                let _ = add(window, target_region, "[result omitted]".to_string(), 5);
            }
        }
    }
}

/// Truncate a file body to `max_tokens` (≈4 chars/token) with a marker, or return
/// it unchanged when no cap is set or it already fits.
fn truncate_file(content: String, max_tokens: Option<usize>) -> String {
    match max_tokens {
        Some(max) => {
            let approx_chars = max * 4;
            if content.len() > approx_chars {
                let head: String = content.chars().take(approx_chars).collect();
                format!("{head}\n\n[... truncated at {max} tokens ...]")
            } else {
                content
            }
        }
        None => content,
    }
}

/// File tracking: for each `read_file`/`write_file` result (per the stage's
/// [`FileTrackingConfig`](leviath_core::blueprint::FileTrackingConfig)), upsert
/// the file body into the configured HashMap region (keyed by path, so re-reads
/// de-dup) and replace the inline tool result with a short reference — keeping
/// large file bodies out of the rolling conversation. No-op unless the region
/// exists and is a HashMap. `read_file`'s body is the result; `write_file`'s is
/// its `content` argument (no re-read needed in the ECS).
fn apply_file_tracking(
    window: &mut ContextWindow,
    ft: &leviath_core::blueprint::FileTrackingConfig,
    tool_calls: &[crate::components::ToolCall],
    merged: &mut [(String, String)],
) {
    let is_hashmap = window
        .get_region(&ft.region)
        .is_some_and(|r| matches!(r.kind, leviath_core::RegionKind::HashMap { .. }));
    if !is_hashmap {
        return;
    }
    for (call, (_id, result)) in tool_calls.iter().zip(merged.iter_mut()) {
        if result.starts_with("[error]") || result.starts_with("[denied]") {
            continue;
        }
        let Some(path) = call.arguments.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        let (body, verb) = match call.name.as_str() {
            "read_file" if ft.track_reads => (result.clone(), "stored"),
            "write_file" if ft.track_writes => {
                match call.arguments.get("content").and_then(|v| v.as_str()) {
                    Some(c) => (c.to_string(), "written"),
                    None => continue,
                }
            }
            _ => continue,
        };
        let body = truncate_file(body, ft.max_file_tokens);
        let tokens = body.len() / 4 + 1;
        window
            .get_region_mut(&ft.region)
            .expect("region presence checked above")
            .upsert_by_key(path, body, tokens)
            .ok();
        *result = format!(
            "File {verb} in [{}] → ### [{}] ({} tokens). Reference it there; do not re-read this path.",
            ft.region, path, tokens
        );
    }
}

/// Tool-collect system: drain finished tool batches and apply them. Results are
/// written into the agent's context window (routing/truncation/taint honored)
/// and the agent loops back to `ReadyToInfer`. Outcomes for agents no longer
/// `AwaitingTools` (cancelled/despawned) are dropped.
#[allow(clippy::type_complexity)]
pub fn collect_tools(
    mut results: ResMut<ToolResults>,
    mut agents: Query<
        (
            &mut ContextWindow,
            &crate::components::InferenceResult,
            Option<&crate::components::ToolResultRoutingComponent>,
            Option<&ToolSensitivities>,
            Option<&ContextToolResults>,
            Option<&StageCursor>,
            Option<&mut StageIoBuffer>,
            Option<&AgentBlueprint>,
        ),
        With<AwaitingTools>,
    >,
    mut commands: Commands,
) {
    while let Ok(outcome) = results.0.try_recv() {
        let Ok((
            mut window,
            infer,
            routing,
            sensitivities,
            context_results,
            cursor,
            buffer,
            blueprint,
        )) = agents.get_mut(outcome.entity)
        else {
            continue; // stale: agent cancelled/despawned since dispatch
        };
        // Merge the inline context-tool results (if any) with the lane results,
        // ordered by the original tool calls.
        let mut parts = outcome.results;
        if let Some(ctx) = context_results {
            parts.extend(ctx.0.iter().cloned());
        }
        let mut merged = merge_in_call_order(&infer.tool_calls, &parts);
        // File tracking: sync read/write results into the configured HashMap
        // region and replace the inline result with a reference (de-dup context).
        if let Some(ft) = blueprint.and_then(|bp| bp.0.file_tracking.as_ref()) {
            apply_file_tracking(&mut window, ft, &infer.tool_calls, &mut merged);
        }
        // Buffer one readable `[tool] name: result` line per call for the stage's
        // logs (merged is in call order, so it zips with the calls by index).
        if let Some(mut buffer) = buffer {
            let idx = cursor.map_or(0, |c| c.index);
            for (call, (_id, result)) in infer.tool_calls.iter().zip(merged.iter()) {
                buffer.logs.push((
                    idx,
                    format!("[tool] {}: {}", call.name, one_line(result, 200)),
                ));
            }
        }
        apply_tool_results(
            &mut window,
            &infer.response,
            &infer.tool_calls,
            &merged,
            routing.map(|c| &c.routing),
            sensitivities.map(|s| &s.0),
        );
        commands
            .entity(outcome.entity)
            .remove::<AwaitingTools>()
            .remove::<ContextToolResults>()
            .insert(ReadyToInfer);
    }
}

// ─── Compaction (LLM context summarization) ──────────────────────────────────

/// Per-agent compaction configuration; its presence opts the agent into
/// automatic eviction + LLM compaction before each inference (mirrors the
/// imperative loop's `Option<&CompactionConfig>`).
#[derive(Component, Clone)]
pub struct CompactionSettings(pub leviath_core::CompactionConfig);

/// A compaction job (LLM summarization) is in flight; the agent is held out of
/// inference until its summaries land.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct AwaitingCompaction;

/// The receiving end of the compaction-outcomes channel, as a world resource.
/// (The sending end lives in [`InferenceStage::compaction_outcomes`].)
#[derive(Resource)]
pub struct CompactionResults(pub UnboundedReceiver<CompactionOutcome>);

/// The eviction threshold (fraction of budget) at which compaction kicks in —
/// the same 0.9 the imperative `evict_and_compact` uses.
const EVICTION_THRESHOLD: f32 = 0.9;

/// Compaction-dispatch system: for each `ReadyToInfer` agent with
/// [`CompactionSettings`] whose window is over the eviction threshold, do the
/// synchronous eviction inline; if that surfaces regions needing LLM
/// summarization (and content to summarize), build one request per region,
/// acquire a permit for the compaction model, spawn the job, and hold the agent
/// as `AwaitingCompaction`. Anything that can't proceed (under threshold, nothing
/// to summarize, provider missing, pool full) simply leaves the agent
/// `ReadyToInfer` so inference proceeds — compaction is best-effort. (Ported from
/// `AgentEngine::evict_and_compact`.)
#[allow(clippy::type_complexity)]
pub fn dispatch_compaction(
    mut agents: Query<
        (Entity, &AgentState, &mut ContextWindow, &CompactionSettings),
        (With<ReadyToInfer>, Without<AwaitingCompaction>),
    >,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    mut commands: Commands,
) {
    for (entity, state, mut window, settings) in agents.iter_mut() {
        if state.status != AgentStatus::Active {
            continue; // paused / waiting / cancelled — don't start new work
        }
        if !window.needs_eviction(EVICTION_THRESHOLD) {
            continue; // under threshold — nothing to do
        }
        let target_free = window.max_tokens / 10;
        let Ok(eviction) = window.try_evict(target_free) else {
            continue; // couldn't evict — proceed to inference as-is
        };

        // Build a summarize request per region that both needs compaction and
        // has content to summarize.
        let config = &settings.0;
        let mut requests = Vec::new();
        for region_name in &eviction.needs_compaction {
            // The names come from `try_evict`'s own scan of `window.regions`, and
            // nothing between there and here mutates the region set, so the region
            // is guaranteed present.
            let region = window
                .get_region(region_name)
                .expect("needs_compaction region present: named by try_evict's own scan");
            let content: String = region
                .content
                .iter()
                .map(|e| e.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            if content.is_empty() {
                continue; // nothing to summarize (e.g. token-only placeholder)
            }
            requests.push((
                region_name.clone(),
                compaction_request(config, &content, region_name),
            ));
        }
        if requests.is_empty() {
            continue; // sync eviction was enough (or nothing summarizable)
        }

        let Some(provider) = providers.0.get(&config.provider).cloned() else {
            continue; // compaction provider not registered — skip, non-fatal
        };
        let Some(permit) = stage.pools.try_acquire(&config.model) else {
            continue; // pool full — skip compaction this round
        };

        stage.runtime.spawn(run_compaction_job(
            CompactionJob {
                entity,
                provider,
                requests,
                permit,
            },
            stage.compaction_outcomes.clone(),
            stage.wake.clone(),
        ));
        commands
            .entity(entity)
            .remove::<ReadyToInfer>()
            .insert(AwaitingCompaction);
    }
}

/// Compaction-collect system: drain finished compaction jobs and apply each
/// summary into its paired `CompactHistory` region, clearing the summarized
/// source region. A provider error leaves the context untouched (best-effort).
/// Either way the agent returns to `ReadyToInfer`. (Ported from the storage tail
/// of `AgentEngine::compact_region`.)
pub fn collect_compaction(
    mut results: ResMut<CompactionResults>,
    mut agents: Query<&mut ContextWindow, With<AwaitingCompaction>>,
    mut commands: Commands,
) {
    while let Ok(outcome) = results.0.try_recv() {
        let Ok(mut window) = agents.get_mut(outcome.entity) else {
            continue; // stale: agent cancelled/despawned since dispatch
        };
        if let Ok(summaries) = outcome.result {
            for (region_name, summary) in summaries {
                let summary_tokens = summary.len() / 4;
                let history = window
                    .regions
                    .iter()
                    .find(|r| {
                        matches!(&r.kind, leviath_core::RegionKind::CompactHistory { source_region }
                            if source_region == &region_name)
                    })
                    .map(|r| r.name.clone());
                if let Some(history_name) = history {
                    let _ = window.add_to_region(&history_name, summary, summary_tokens);
                }
                if let Some(region) = window.get_region_mut(&region_name) {
                    region.clear();
                }
            }
            window.current_tokens = window.calculate_tokens();
        }
        commands
            .entity(outcome.entity)
            .remove::<AwaitingCompaction>()
            .insert(ReadyToInfer);
    }
}

/// Build the summarize [`InferenceRequest`] for one region's content.
fn compaction_request(
    config: &leviath_core::CompactionConfig,
    content: &str,
    region_name: &str,
) -> InferenceRequest {
    InferenceRequest {
        system: vec![],
        messages: vec![
            leviath_providers::Message {
                role: "system".to_string(),
                content: config.system_prompt().to_string().into(),
                cache_breakpoint: false,
            },
            leviath_providers::Message {
                role: "user".to_string(),
                content: config.user_prompt(content, region_name).into(),
                cache_breakpoint: false,
            },
        ],
        model: config.model.clone(),
        max_tokens: config.max_summary_tokens,
        temperature: config.temperature,
        tools: Vec::new(),
        extra: serde_json::Value::Null,
    }
}

// ─── Edge transforms (context reshaping on stage transitions) ────────────────

/// Regions an edge transform asked to LLM-compact after a transition, awaiting
/// the compaction lane (drained by [`dispatch_edge_compact`]).
#[derive(Component, Debug, Clone)]
pub struct PendingEdgeCompact(pub Vec<String>);

/// Whether a region kind is "stage-specific" — eligible for an edge transform to
/// clear or compact. The always-preserved kinds (pinned identity, compaction
/// history, hashmap stores) are never touched.
fn is_stage_specific(kind: &leviath_core::RegionKind) -> bool {
    !matches!(
        kind,
        leviath_core::RegionKind::Pinned
            | leviath_core::RegionKind::CompactHistory { .. }
            | leviath_core::RegionKind::HashMap { .. }
    )
}

/// Apply an edge transform's **synchronous** effects to the outgoing window
/// (clearing stage-specific / named regions) and return the names of regions the
/// caller should hand to the LLM compaction lane. (Ported from the deleted
/// `graph::apply_edge_transform`; `Direct` on a linear/chosen edge carries context
/// as-is.)
pub(crate) fn apply_edge_transform(
    window: &mut ContextWindow,
    transform: &leviath_core::blueprint::EdgeTransform,
) -> Vec<String> {
    use leviath_core::blueprint::EdgeTransform;
    match transform {
        EdgeTransform::Direct => Vec::new(),
        EdgeTransform::Clear => {
            window
                .regions
                .iter_mut()
                .filter(|r| is_stage_specific(&r.kind))
                .for_each(|r| r.clear());
            window.current_tokens = window.calculate_tokens();
            Vec::new()
        }
        EdgeTransform::Compact { .. } => window
            .regions
            .iter()
            .filter(|r| is_stage_specific(&r.kind) && !r.content.is_empty())
            .map(|r| r.name.clone())
            .collect(),
        EdgeTransform::Custom {
            carry,
            compact,
            clear,
            ..
        } => {
            clear
                .iter()
                .filter(|n| !carry.contains(n))
                .for_each(|name| {
                    window
                        .get_region_mut(name)
                        .into_iter()
                        .for_each(|r| r.clear());
                });
            window.current_tokens = window.calculate_tokens();
            compact
                .iter()
                .filter(|n| !carry.contains(n))
                .filter(|n| window.get_region(n).is_some_and(|r| !r.content.is_empty()))
                .cloned()
                .collect()
        }
    }
}

/// Edge-compaction dispatch: for each `ReadyToInfer` agent with a
/// [`PendingEdgeCompact`] (an edge transform requested LLM summarization), spawn a
/// compaction job for the named regions (reusing the compaction lane) and hold the
/// agent `AwaitingCompaction`. If the agent has no compaction config, nothing to
/// summarize, or no provider/permit, the request is dropped and the agent proceeds
/// to inference un-compacted (memory-pressure compaction still applies later).
#[allow(clippy::type_complexity)]
pub fn dispatch_edge_compact(
    mut agents: Query<
        (
            Entity,
            &AgentState,
            &ContextWindow,
            &PendingEdgeCompact,
            Option<&CompactionSettings>,
        ),
        (With<ReadyToInfer>, Without<AwaitingCompaction>),
    >,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    mut commands: Commands,
) {
    for (entity, state, window, pending, settings) in agents.iter_mut() {
        if state.status != AgentStatus::Active {
            continue; // paused / waiting / cancelled — don't start new work
        }
        let started = settings
            .and_then(|s| {
                let config = &s.0;
                let requests = build_edge_compact_requests(window, &pending.0, config)?;
                let provider = providers.0.get(&config.provider).cloned()?;
                let permit = stage.pools.try_acquire(&config.model)?;
                stage.runtime.spawn(run_compaction_job(
                    CompactionJob {
                        entity,
                        provider,
                        requests,
                        permit,
                    },
                    stage.compaction_outcomes.clone(),
                    stage.wake.clone(),
                ));
                Some(())
            })
            .is_some();

        let mut ec = commands.entity(entity);
        ec.remove::<PendingEdgeCompact>();
        if started {
            ec.remove::<ReadyToInfer>().insert(AwaitingCompaction);
        }
    }
}

/// Build the per-region summarize requests for an edge compaction, or `None` when
/// none of the named regions have content to summarize.
fn build_edge_compact_requests(
    window: &ContextWindow,
    regions: &[String],
    config: &leviath_core::CompactionConfig,
) -> Option<Vec<(String, InferenceRequest)>> {
    let requests: Vec<(String, InferenceRequest)> = regions
        .iter()
        .filter_map(|name| {
            let region = window.get_region(name)?;
            let content = region
                .content
                .iter()
                .map(|e| e.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            (!content.is_empty())
                .then(|| (name.clone(), compaction_request(config, &content, name)))
        })
        .collect();
    (!requests.is_empty()).then_some(requests)
}

// ─── Persistence (per-agent snapshot writing) ────────────────────────────────

/// Debounce watermark: the (iteration, stage index, status) last persisted for an
/// agent. A snapshot is written only when one of these changes, so the world
/// writes on meaningful progress rather than every tick. `None` until the first
/// snapshot, so a freshly-spawned agent is always written once.
#[derive(Component, Default)]
pub struct PersistWatermark {
    last: Option<(usize, usize, leviath_core::run_meta::RunStatus)>,
}

/// The sending end of the persistence I/O lane (the receiving end is drained by
/// [`crate::persistence_bridge::persistence_worker`]).
#[derive(Resource)]
pub struct PersistenceStage(pub UnboundedSender<PersistJob>);

/// Persistence-dispatch system: for each agent carrying run metadata whose
/// (iteration, stage, status) has changed since its last snapshot, build the
/// `meta.json` + `context.json` value snapshot and hand it to the persistence
/// lane. Fire-and-forget — no result to collect; the single-worker lane keeps a
/// given agent's writes ordered. Agents without [`RunMetadata`] aren't persisted.
#[allow(clippy::type_complexity)]
/// Interaction-status reflection system: mirror the shared [`InteractionHub`]'s
/// open requests into agent status so a blocked agent shows as `Waiting` (and
/// the dashboard / `lev ps` surface its prompt) instead of a silent `Active`.
///
/// An agent's `ask_user_*` / tool-approval / plan-approval call blocks deep in
/// the async tool lane, invisible to the ECS — which otherwise leaves the agent
/// `Active` with meta.json written `running`, so the dashboard (gated on
/// `WaitingInput`) never shows the prompt and the run looks frozen. This system
/// closes that gap: an agent whose id has an open hub request flips
/// `Active → Waiting` (tagged [`AwaitingInteraction`]); when the request clears
/// it flips back `Waiting → Active`. Fan-out waiting ([`FanOutWaiting`]) is left
/// untouched. No-op when the world has no hub resource (test worlds).
pub fn reflect_interaction_status(
    hub: Option<Res<InteractionHub>>,
    mut agents: Query<
        (Entity, &mut AgentState, Option<&AwaitingInteraction>),
        Without<FanOutWaiting>,
    >,
    mut commands: Commands,
) {
    let Some(hub) = hub else { return };
    let pending: std::collections::HashSet<String> =
        hub.pending().into_iter().map(|(id, _)| id).collect();
    for (entity, mut state, marked) in agents.iter_mut() {
        match (pending.contains(&state.agent_id), marked.is_some()) {
            // Newly blocked on a prompt: surface it as Waiting.
            (true, false) => {
                if state.status == AgentStatus::Active {
                    state.status = AgentStatus::Waiting;
                    commands.entity(entity).insert(AwaitingInteraction);
                }
            }
            // Request cleared (answered / cancelled): return to Active, unless
            // the agent has since reached a terminal status.
            (false, true) => {
                commands.entity(entity).remove::<AwaitingInteraction>();
                if state.status == AgentStatus::Waiting {
                    state.status = AgentStatus::Active;
                }
            }
            _ => {}
        }
    }
}

/// Reconcile a [`StageLedger`]'s per-stage `status` + timestamps against the
/// agent's current stage index and status: stages before the cursor are
/// `Complete`, the cursor stage takes the mapped agent status, later stages stay
/// `Pending`. `started_at`/`ended_at` are stamped once and never overwritten, so
/// repeated calls are idempotent.
fn reconcile_stage_ledger(
    ledger: &mut StageLedger,
    cursor_index: usize,
    status: &AgentStatus,
    now: i64,
) {
    use leviath_core::run_meta::StageRunStatus;
    let active = crate::persistence::stage_status_from(status);
    for rec in ledger.0.iter_mut() {
        match rec.index.cmp(&cursor_index) {
            std::cmp::Ordering::Less => {
                if rec.started_at.is_none() {
                    rec.started_at = Some(now);
                }
                rec.status = StageRunStatus::Complete;
                if rec.ended_at.is_none() {
                    rec.ended_at = Some(now);
                }
            }
            std::cmp::Ordering::Equal => {
                if rec.started_at.is_none() {
                    rec.started_at = Some(now);
                }
                if active == StageRunStatus::Complete && rec.ended_at.is_none() {
                    rec.ended_at = Some(now);
                }
                rec.status = active.clone();
            }
            std::cmp::Ordering::Greater => {
                rec.status = StageRunStatus::Pending;
            }
        }
    }
}

#[allow(clippy::type_complexity)]
pub fn dispatch_persistence(
    mut agents: Query<(
        &RunMetadata,
        &AgentState,
        &ContextWindow,
        &StageCursor,
        &TokenTotals,
        &mut PersistWatermark,
        Option<&mut StageLedger>,
        Option<&mut StageIoBuffer>,
    )>,
    stage: Res<PersistenceStage>,
) {
    for (md, state, window, cursor, totals, mut watermark, mut ledger, buffer) in agents.iter_mut()
    {
        let now = chrono::Utc::now().timestamp();

        // Reconcile the stage ledger every persist tick so status/timestamps track
        // the agent regardless of whether the run-level watermark changed.
        if let Some(ledger) = ledger.as_deref_mut() {
            reconcile_stage_ledger(ledger, cursor.index, &state.status, now);
        }

        // Always flush any buffered per-stage output/log lines.
        let (output_appends, log_appends) = match buffer {
            Some(mut buf) => (
                std::mem::take(&mut buf.output),
                std::mem::take(&mut buf.logs),
            ),
            None => (Vec::new(), Vec::new()),
        };
        let has_appends = !output_appends.is_empty() || !log_appends.is_empty();

        let status = crate::persistence::run_status_from(&state.status);
        let current = (state.iteration, cursor.index, status);
        let watermark_changed = watermark.last.as_ref() != Some(&current);
        if !watermark_changed && !has_appends {
            continue; // nothing meaningful changed and nothing buffered
        }
        if watermark_changed {
            watermark.last = Some(current);
        }

        let meta = build_run_meta(md, state, totals, cursor.index, now);
        let context = build_context_snapshot(window, &state.current_stage);
        let stages = ledger.as_deref().map(|l| l.0.clone()).unwrap_or_default();
        let _ = stage.0.send(PersistJob {
            run_id: md.run_id.clone(),
            meta,
            context,
            stages,
            output_appends,
            log_appends,
        });
    }
}

/// The receiving end of the world's inbound-message channel. Clients (the
/// control API) send `AgentMessage`s here; the delivery system routes and
/// delivers them.
#[derive(Resource)]
pub struct MessageIntake(pub UnboundedReceiver<AgentMessage>);

/// Message-delivery system: route inbound messages to their target agents'
/// inboxes (by agent id), then deliver each inbox into the agent's context
/// window — but only for agents whose current stage accepts messages; otherwise
/// the messages wait in the inbox for a stage that does. Ported from
/// `AgentEngine::process_messages` / `deliver_inbox_messages`.
pub fn deliver_messages(
    mut intake: ResMut<MessageIntake>,
    mut agents: Query<(&AgentState, &mut MessageInbox, &mut ContextWindow)>,
) {
    // Route inbound channel messages to their target agent's inbox.
    let mut incoming = Vec::new();
    while let Ok(msg) = intake.0.try_recv() {
        incoming.push(msg);
    }
    for msg in incoming {
        for (state, mut inbox, _) in agents.iter_mut() {
            if state.agent_id == msg.agent_id {
                inbox.push(msg.clone());
                break;
            }
        }
        // Unmatched target ⇒ dropped (agent no longer exists).
    }

    // Deliver inboxes into context windows for agents that accept messages.
    for (state, mut inbox, mut window) in agents.iter_mut() {
        if !state.accepts_messages {
            continue; // hold until a stage that accepts messages
        }
        for msg in inbox.drain_all() {
            let region = msg.target_region.as_deref().unwrap_or("conversation");
            let tokens = msg.content.len() / 4 + 1;
            let _ = window.add_typed_entry(
                region,
                leviath_core::EntryKind::UserMessage,
                msg.content.clone(),
                tokens,
            );
        }
    }
}

// ─── Stage transition ────────────────────────────────────────────────────────

/// The agent's blueprint (its stage graph), as a component.
#[derive(Component, Debug, Clone)]
pub struct AgentBlueprint(pub leviath_core::Blueprint);

/// The index of the agent's current stage within its blueprint.
#[derive(Component, Debug, Clone, Copy)]
pub struct StageCursor {
    /// Current stage index.
    pub index: usize,
}

/// Pre-resolved [`StageInference`] for every stage of the agent's blueprint,
/// built once when the agent is spawned (the CLI resolves each stage's provider,
/// model, and tool definitions). The transition system swaps the agent's
/// `StageInference` to the entry for its new stage by index.
#[derive(Component, Debug, Clone)]
pub struct StageInferences(pub Vec<StageInference>);

/// How many times the agent has entered each stage (for `max_revisits`).
#[derive(Component, Debug, Clone, Default)]
pub struct VisitCounts(pub std::collections::HashMap<String, usize>);

/// Pre-resolved per-stage setup, applied by `enter_stage` when an agent enters
/// a stage: inference parameters, tool-result routing, whether the stage accepts
/// live user input, an optional stage-specific context layout, and an optional
/// system prompt. Built once per stage when the agent is spawned (mirrors
/// [`StageInferences`]) so stage entry stays synchronous and query-friendly.
/// (Ported from the imperative loop's per-stage setup in the CLI executor.)
#[derive(Clone)]
pub struct StageSetup {
    /// Per-stage inference config (temperature / max output tokens).
    pub inference_config: InferenceConfig,
    /// Optional per-stage tool-result routing.
    pub routing: Option<leviath_core::ToolResultRouting>,
    /// Whether the stage delivers live user messages to the agent.
    pub accepts_messages: bool,
    /// Optional stage-specific context layout to swap to on entry.
    pub context_layout: Option<leviath_core::ContextLayout>,
    /// Optional stage instructions injected as pinned context on entry.
    pub system_prompt: Option<String>,
}

/// Pre-resolved [`StageSetup`] for every stage of the agent's blueprint.
#[derive(Component, Clone)]
pub struct StageSetups(pub Vec<StageSetup>);

/// The stage completed with multiple candidate edges (or a single edge the stage
/// may decline); an LLM must choose. Holds the choosable edges for the async
/// transition-choice system.
#[derive(Component, Debug, Clone)]
pub struct AwaitingTransitionChoice(pub Vec<leviath_core::blueprint::TransitionEdge>);

/// The outcome of synchronously resolving a completed stage's transition.
enum StageResolution {
    /// No valid outgoing transition — the agent is done.
    Terminal,
    /// The stage errored and has no `error` edge — terminate the run as errored,
    /// preserving the error status the collect system already set.
    TerminalError,
    /// Advance to this stage index, applying the edge's context transform.
    Next(usize, leviath_core::blueprint::EdgeTransform),
    /// Multiple candidate edges — an LLM must choose among them.
    Choose(Vec<leviath_core::blueprint::TransitionEdge>),
}

/// Find the first available edge with the given `condition` (e.g. `Error` or
/// `MaxIterations`) whose target exists and hasn't exhausted its revisit budget.
fn find_conditioned_edge(
    blueprint: &leviath_core::Blueprint,
    stage: &leviath_core::Stage,
    visits: &std::collections::HashMap<String, usize>,
    condition: leviath_core::blueprint::TransitionCondition,
) -> Option<(usize, leviath_core::blueprint::EdgeTransform)> {
    let transitions = stage.transitions.as_ref()?;
    transitions.values().find_map(|edge| {
        if edge.condition != condition {
            return None;
        }
        let idx = blueprint
            .stages
            .iter()
            .position(|s| s.name == edge.target)?;
        let within_budget = match blueprint.stages[idx].max_revisits {
            Some(max) => visits.get(&edge.target).copied().unwrap_or(0) <= max,
            None => true,
        };
        within_budget.then(|| (idx, edge.transform.clone()))
    })
}

/// Max-iterations guard: for each `ReadyToInfer` agent whose per-stage inference
/// count has reached the stage's `max_iterations`, end the stage (routing to a
/// `max_iterations` edge if one exists, else a normal transition) instead of
/// running another inference. Ported from the imperative `run_autonomous` cap.
pub fn enforce_max_iterations(
    agents: Query<
        (
            Entity,
            &AgentState,
            &AgentBlueprint,
            &StageCursor,
            &StageProgress,
        ),
        With<ReadyToInfer>,
    >,
    mut commands: Commands,
) {
    for (entity, state, bp, cursor, progress) in agents.iter() {
        if state.status != AgentStatus::Active {
            continue;
        }
        let max = bp.0.stages[cursor.index].max_iterations.unwrap_or(0);
        if max > 0 && progress.iterations >= max {
            commands
                .entity(entity)
                .remove::<ReadyToInfer>()
                .insert(ResolveTransition)
                .insert(StageOutcome::MaxIterations);
        }
    }
}

/// Resolve the next stage for a normally-completed stage without any LLM call.
/// (Ported from the synchronous portion of `graph::resolve_transition`; the
/// `Error`/`MaxIterations` auto-transitions don't apply to a normal completion,
/// and the LLM-choice case is returned as [`StageResolution::Choose`].)
fn resolve_transition_sync(
    blueprint: &leviath_core::Blueprint,
    stage: &leviath_core::Stage,
    stage_idx: usize,
    visits: &std::collections::HashMap<String, usize>,
) -> StageResolution {
    use leviath_core::blueprint::TransitionCondition;
    match &stage.transitions {
        None => {
            if stage_idx + 1 < blueprint.stages.len() {
                // A linear fall-through carries context as-is (Direct).
                StageResolution::Next(
                    stage_idx + 1,
                    leviath_core::blueprint::EdgeTransform::Direct,
                )
            } else {
                StageResolution::Terminal
            }
        }
        Some(transitions) => {
            if transitions.is_empty() {
                return StageResolution::Terminal;
            }
            // Filter edges whose target hasn't exhausted its revisit budget.
            let available: Vec<&leviath_core::blueprint::TransitionEdge> = transitions
                .values()
                .filter(|e| match blueprint.find_stage(&e.target) {
                    Some(ts) => match ts.max_revisits {
                        Some(max) => visits.get(&e.target).copied().unwrap_or(0) <= max,
                        None => true,
                    },
                    None => false, // unknown target
                })
                .collect();
            // Only Always/LlmChoice edges are auto/LLM-followable on completion.
            let choosable: Vec<&leviath_core::blueprint::TransitionEdge> = available
                .into_iter()
                .filter(|e| {
                    matches!(
                        e.condition,
                        TransitionCondition::Always | TransitionCondition::LlmChoice
                    )
                })
                .collect();
            match choosable.len() {
                0 => StageResolution::Terminal,
                1 if !stage.allow_complete => {
                    let idx = blueprint
                        .stages
                        .iter()
                        .position(|s| s.name == choosable[0].target)
                        .unwrap_or(0);
                    StageResolution::Next(idx, choosable[0].transform.clone())
                }
                _ => StageResolution::Choose(choosable.into_iter().cloned().collect()),
            }
        }
    }
}

/// Marks a parent agent held at a `requires_children` stage boundary until all
/// its spawned sub-agents are terminal. Distinct from `FanOutWaiting` (which is
/// the fan-out split/merge wait).
#[derive(Component, Debug, Clone, Copy)]
pub struct WaitingForChildren;

/// Whether an agent status is terminal (the run/child has finished).
fn is_terminal_status(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Complete | AgentStatus::Error { .. } | AgentStatus::Cancelled
    )
}

/// `requires_children` gate (exclusive, mirrors the fan-out wait): a stage marked
/// `requires_children` may not transition while any of the agent's spawned
/// sub-agents ([`SubAgentChildren`](crate::components::SubAgentChildren)) are
/// still running — the parent is held `Waiting` (`WaitingForChildren`) and
/// resumes (re-inserting `ResolveTransition`, back to `Active`) once every child
/// is terminal.
pub fn gate_requires_children(world: &mut World) {
    use crate::components::SubAgentChildren;

    // Hold: transitioning agents whose stage requires children that aren't done.
    // `&AgentState` in the query guarantees the later `.expect()` never fires.
    let mut candidates: Vec<(Entity, Vec<Entity>)> = Vec::new();
    {
        let mut q = world.query_filtered::<(
            Entity,
            &AgentBlueprint,
            &StageCursor,
            &SubAgentChildren,
            &AgentState,
        ), With<ResolveTransition>>();
        for (e, bp, cursor, children, _) in q.iter(world) {
            if bp.0.stages[cursor.index].requires_children {
                candidates.push((e, children.children.clone()));
            }
        }
    }
    for (entity, children) in candidates {
        let pending = children.iter().any(|&c| {
            world
                .get::<AgentState>(c)
                .is_some_and(|s| !is_terminal_status(&s.status))
        });
        if pending {
            world
                .entity_mut(entity)
                .remove::<ResolveTransition>()
                .insert(WaitingForChildren);
            world
                .get_mut::<AgentState>(entity)
                .expect("held agent has AgentState")
                .status = AgentStatus::Waiting;
        }
    }

    // Resume: held agents whose children have all finished.
    let mut waiting: Vec<(Entity, Vec<Entity>)> = Vec::new();
    {
        let mut q = world.query_filtered::<
            (Entity, Option<&SubAgentChildren>, &AgentState),
            With<WaitingForChildren>,
        >();
        for (e, children, _) in q.iter(world) {
            waiting.push((e, children.map(|c| c.children.clone()).unwrap_or_default()));
        }
    }
    for (entity, children) in waiting {
        let all_done = children.iter().all(|&c| {
            world
                .get::<AgentState>(c)
                .is_none_or(|s| is_terminal_status(&s.status))
        });
        if all_done {
            world
                .entity_mut(entity)
                .remove::<WaitingForChildren>()
                .insert(ResolveTransition);
            world
                .get_mut::<AgentState>(entity)
                .expect("waiting agent has AgentState")
                .status = AgentStatus::Active;
        }
    }
}

/// Default re-entry cap for required-region gating: how many times a stage is
/// re-run to populate an empty `required` region before proceeding anyway (with a
/// warning). Overridable per stage via `max_revisits`.
const DEFAULT_REQUIRED_REENTRY_CAP: usize = 3;

/// Counts how many times the current stage has been re-run to satisfy required
/// context regions. Absent ⇒ 0; reset when a new stage is entered.
#[derive(Component, Debug, Clone, Copy)]
pub struct RequiredReentries(pub usize);

/// Required regions (from the stage's effective layout) still empty at stage end,
/// as `(name, optional custom message)`. Empty when the stage has no
/// context-writing tool (gating a stage that can't populate the region would loop
/// pointlessly). Ported from the imperative `unmet_required_regions`.
fn unmet_required_regions(
    blueprint: &leviath_core::Blueprint,
    stage: &leviath_core::Stage,
    window: &ContextWindow,
) -> Vec<(String, Option<String>)> {
    let can_write = stage
        .available_tools
        .iter()
        .any(|t| t == "context_write" || t == "context_append");
    if !can_write {
        return Vec::new();
    }
    let layout = stage
        .context_layout
        .as_ref()
        .unwrap_or(&blueprint.context_layout);
    layout
        .regions
        .iter()
        .filter(|r| r.required)
        .filter(|r| {
            window
                .get_region(&r.name)
                .map(|reg| reg.content.is_empty())
                .unwrap_or(true)
        })
        .map(|r| (r.name.clone(), r.required_message.clone()))
        .collect()
}

/// Inject a `[System]` nudge into the conversation region for each unmet required
/// region, so the stage re-run tells the agent exactly what to populate.
fn inject_required_region_nudges(window: &mut ContextWindow, unmet: &[(String, Option<String>)]) {
    for (name, msg) in unmet {
        let text = msg.clone().unwrap_or_else(|| {
            format!(
                "Required context region '{name}' is still empty. You must populate it \
                 (e.g. via context_write with region=\"{name}\") before this stage can complete."
            )
        });
        let content = format!("[System] {text}");
        let tokens = content.len() / 4 + 1;
        let _ = window.add_to_region("conversation", content, tokens);
    }
}

/// Required-region gate: before a normally-completed stage transitions, if it can
/// write context and a `required` region is still empty, inject a nudge and re-run
/// the stage (loop back to `ReadyToInfer`) instead of transitioning — bounded by
/// the stage's `max_revisits` (or a default cap), after which
/// it proceeds with a warning. Skipped when the stage ended on an error / max-iter
/// outcome (those transitions take precedence). Ported from the imperative gate.
#[allow(clippy::type_complexity)]
pub fn require_context_regions(
    mut agents: Query<
        (
            Entity,
            &AgentBlueprint,
            &StageCursor,
            &mut ContextWindow,
            Option<&RequiredReentries>,
            Option<&StageOutcome>,
        ),
        With<ResolveTransition>,
    >,
    mut commands: Commands,
) {
    for (entity, bp, cursor, mut window, reentries, outcome) in agents.iter_mut() {
        if outcome.is_some() {
            continue; // error / max-iterations transition takes precedence
        }
        let stage = &bp.0.stages[cursor.index];
        let unmet = unmet_required_regions(&bp.0, stage, &window);
        if unmet.is_empty() {
            continue;
        }
        let cap = stage.max_revisits.unwrap_or(DEFAULT_REQUIRED_REENTRY_CAP);
        let round = reentries.map_or(0, |r| r.0);
        if round >= cap {
            let names: Vec<&str> = unmet.iter().map(|(n, _)| n.as_str()).collect();
            tracing::warn!(
                stage = %stage.name,
                regions = ?names,
                attempts = cap,
                "required context regions still empty after re-run attempts; proceeding"
            );
            continue; // proceed with the transition despite the unmet regions
        }
        inject_required_region_nudges(&mut window, &unmet);
        commands
            .entity(entity)
            .remove::<ResolveTransition>()
            .insert(ReadyToInfer)
            .insert(RequiredReentries(round + 1));
    }
}

/// Transition-resolution system: for each `ResolveTransition` agent, resolve the
/// next stage. Terminal ⇒ mark the agent `Complete`. A single/linear target ⇒
/// enter the new stage (swap its `StageInference`, reset stage progress, bump the
/// visit count) and loop to `ReadyToInfer`. Multiple candidate edges ⇒ hand off
/// to the async transition-choice system via `AwaitingTransitionChoice`.
#[allow(clippy::type_complexity)]
pub fn resolve_transition(
    mut agents: Query<
        (
            Entity,
            &AgentBlueprint,
            &mut StageCursor,
            &mut AgentState,
            &mut StageProgress,
            &StageInferences,
            &StageSetups,
            &mut VisitCounts,
            &mut ContextWindow,
            Option<&StageOutcome>,
        ),
        With<ResolveTransition>,
    >,
    mut commands: Commands,
) {
    use leviath_core::blueprint::TransitionCondition;
    for (
        entity,
        bp,
        mut cursor,
        mut state,
        mut progress,
        stage_infs,
        setups,
        mut visits,
        mut window,
        outcome,
    ) in agents.iter_mut()
    {
        let stage = &bp.0.stages[cursor.index];
        // How the stage ended governs the transition: an error/max-iterations
        // outcome follows its conditioned edge (e.g. → error_recovery) if present.
        let resolution = match outcome {
            Some(StageOutcome::Errored(_)) => {
                find_conditioned_edge(&bp.0, stage, &visits.0, TransitionCondition::Error)
                    .map(|(i, t)| StageResolution::Next(i, t))
                    .unwrap_or(StageResolution::TerminalError)
            }
            Some(StageOutcome::MaxIterations) => {
                find_conditioned_edge(&bp.0, stage, &visits.0, TransitionCondition::MaxIterations)
                    .map(|(i, t)| StageResolution::Next(i, t))
                    .unwrap_or_else(|| {
                        resolve_transition_sync(&bp.0, stage, cursor.index, &visits.0)
                    })
            }
            None => resolve_transition_sync(&bp.0, stage, cursor.index, &visits.0),
        };
        match resolution {
            StageResolution::Terminal => {
                state.status = AgentStatus::Complete;
                commands
                    .entity(entity)
                    .remove::<ResolveTransition>()
                    .remove::<StageOutcome>();
            }
            StageResolution::TerminalError => {
                // Status was set to Error by the collect system; just stop.
                commands
                    .entity(entity)
                    .remove::<ResolveTransition>()
                    .remove::<StageOutcome>();
            }
            StageResolution::Next(idx, transform) => {
                // Reshape the outgoing context per the edge transform before the
                // new stage's layout/prompt setup.
                let to_compact = apply_edge_transform(&mut window, &transform);
                let setup = &setups.0[idx];
                match enter_stage(
                    idx,
                    &bp.0,
                    &mut cursor,
                    &mut state,
                    &mut progress,
                    &mut visits,
                    setup,
                    &mut window,
                ) {
                    Ok(()) => {
                        // Entering a stage is active work; clears a prior error
                        // status when recovering down an `error` edge.
                        state.status = AgentStatus::Active;
                        let name = bp.0.stages[idx].name.clone();
                        let mut ec = commands.entity(entity);
                        ec.remove::<ResolveTransition>().remove::<StageOutcome>();
                        attach_stage_components(ec, stage_infs.0[idx].clone(), setup, idx, name);
                        if !to_compact.is_empty() {
                            commands
                                .entity(entity)
                                .insert(PendingEdgeCompact(to_compact));
                        }
                    }
                    Err(message) => {
                        state.status = AgentStatus::Error { message };
                        commands
                            .entity(entity)
                            .remove::<ResolveTransition>()
                            .remove::<StageOutcome>();
                    }
                }
            }
            StageResolution::Choose(edges) => {
                commands
                    .entity(entity)
                    .remove::<ResolveTransition>()
                    .remove::<StageOutcome>()
                    .insert(AwaitingTransitionChoice(edges));
            }
        }
    }
}

/// Enter the stage at `idx`: update the cursor + current-stage name, reset
/// per-stage progress, bump the visit count, set `accepts_messages`, and apply the
/// stage's context setup — swap to its layout (if any) and (re)inject its system
/// prompt as pinned `[Stage instructions: …]` context, replacing the previous
/// stage's. (Ported from the imperative loop's per-stage setup.)
///
/// Returns `Err` only when the system prompt doesn't fit its region — the same
/// hard failure the imperative loop raises; the caller marks the agent `Error`.
#[allow(clippy::too_many_arguments)]
fn enter_stage(
    idx: usize,
    blueprint: &leviath_core::Blueprint,
    cursor: &mut StageCursor,
    state: &mut AgentState,
    progress: &mut StageProgress,
    visits: &mut VisitCounts,
    setup: &StageSetup,
    window: &mut ContextWindow,
) -> Result<(), String> {
    cursor.index = idx;
    let name = blueprint.stages[idx].name.clone();
    state.current_stage = name.clone();
    state.accepts_messages = setup.accepts_messages;
    *progress = StageProgress::default();
    *visits.0.entry(name).or_insert(0) += 1;

    apply_stage_context(setup, window)
}

/// Apply a stage's context setup to a window: swap to the stage's layout (if any)
/// and (re)inject its system prompt as pinned `[Stage instructions: …]` context,
/// clearing any previous stage's first. Returns `Err` only when the prompt
/// doesn't fit its region. Shared by [`enter_stage`] (transitions) and
/// [`build_agent`] (the first stage, at spawn).
fn apply_stage_context(setup: &StageSetup, window: &mut ContextWindow) -> Result<(), String> {
    if let Some(layout) = &setup.context_layout {
        crate::context_setup::apply_layout(window, layout);
    }

    // Inject stage instructions into the first pinned region (cacheable), or the
    // conversation region if there is none — clearing any prior stage's first.
    let target = window
        .regions
        .iter()
        .find(|r| matches!(r.kind, leviath_core::RegionKind::Pinned))
        .map(|r| r.name.clone())
        .unwrap_or_else(|| "conversation".to_string());
    if let Some(region) = window.regions.iter_mut().find(|r| r.name == target) {
        region.remove_entries_by_prefix("[Stage instructions:");
    }
    if let Some(sp) = &setup.system_prompt {
        let content = format!("[Stage instructions: {sp}]");
        let tokens = content.len() / 4 + 1;
        window
            .add_to_region(&target, content, tokens)
            .map_err(|e| {
                format!(
                    "stage system prompt (~{tokens} tokens) does not fit context region \
                 '{target}': {e}. Increase that region's max_tokens (or shorten the prompt)."
                )
            })?;
    }
    Ok(())
}

/// Finish a successful stage entry: attach the new stage's inference config,
/// tool-result routing (present ⇒ insert, absent ⇒ clear the stale one), and its
/// pre-resolved [`StageInference`], then mark the agent `ReadyToInfer`. Shared by
/// both the synchronous and LLM-choice transition paths.
fn attach_stage_components(
    mut entity: bevy_ecs::system::EntityCommands,
    stage_inf: StageInference,
    setup: &StageSetup,
    stage_index: usize,
    stage_name: String,
) {
    entity
        .insert(stage_inf)
        .insert(setup.inference_config.clone())
        .insert(StageJustEntered {
            index: stage_index,
            name: stage_name,
        })
        // A fresh stage re-arms its interaction points + required-region gate.
        .remove::<crate::interaction_points::InteractionPointCursor>()
        .remove::<crate::interaction_points::InteractionPointRounds>()
        .remove::<RequiredReentries>()
        .insert(ReadyToInfer);
    match &setup.routing {
        Some(routing) => {
            entity.insert(crate::components::ToolResultRoutingComponent {
                routing: routing.clone(),
            });
        }
        None => {
            entity.remove::<crate::components::ToolResultRoutingComponent>();
        }
    }
}

/// Force an agent into the stage at `target_idx` via direct world access — the
/// same effect as [`resolve_transition`]'s linear-`Next` arm, but callable from
/// an exclusive system (e.g. the fan-out collector jumping to its `merge_stage`)
/// or the daemon (spawning a fan-out worker directly at its worker stage) where no
/// [`Commands`] queue is available. On a system-prompt overflow the agent is
/// marked `Error`, mirroring the transition systems.
pub fn force_transition(world: &mut World, entity: Entity, target_idx: usize) {
    // Phase 1 (scoped borrow): mutate the agent's own state via `enter_stage`,
    // returning the components Phase 2 must insert — or `None` if the agent is
    // gone or its system prompt overflowed (already marked `Error` in-place).
    let attach: Option<(StageInference, StageSetup, String)> = {
        let mut q = world.query::<(
            &AgentBlueprint,
            &mut StageCursor,
            &mut AgentState,
            &mut StageProgress,
            &StageInferences,
            &StageSetups,
            &mut VisitCounts,
            &mut ContextWindow,
        )>();
        let Ok((
            bp,
            mut cursor,
            mut state,
            mut progress,
            stage_infs,
            setups,
            mut visits,
            mut window,
        )) = q.get_mut(world, entity)
        else {
            return; // agent despawned
        };
        let setup = setups.0[target_idx].clone();
        let stage_inf = stage_infs.0[target_idx].clone();
        let name = bp.0.stages[target_idx].name.clone();
        let bp = bp.0.clone();
        match enter_stage(
            target_idx,
            &bp,
            &mut cursor,
            &mut state,
            &mut progress,
            &mut visits,
            &setup,
            &mut window,
        ) {
            Ok(()) => Some((stage_inf, setup, name)),
            Err(message) => {
                state.status = AgentStatus::Error { message };
                None
            }
        }
    };

    // Phase 2 (borrow released): attach the new stage's components directly.
    let Some((stage_inf, setup, name)) = attach else {
        return;
    };
    let mut em = world.entity_mut(entity);
    em.insert(stage_inf)
        .insert(setup.inference_config.clone())
        .insert(StageJustEntered {
            index: target_idx,
            name,
        })
        .insert(ReadyToInfer);
    match &setup.routing {
        Some(routing) => {
            em.insert(crate::components::ToolResultRoutingComponent {
                routing: routing.clone(),
            });
        }
        None => {
            em.remove::<crate::components::ToolResultRoutingComponent>();
        }
    }
}

/// A blueprint stage resolved to a concrete provider, model, and effective tool
/// set — the per-stage input to [`spawn_agent`]. The caller (CLI / daemon) owns
/// the model-selection policy (overrides, availability, user defaults) and tool
/// filtering; the runtime just turns the result into agent data.
pub struct ResolvedStage {
    /// The provider to call for this stage.
    pub provider_name: String,
    /// The resolved model name.
    pub model: String,
    /// The effective tool set for this stage (already filtered).
    pub tools: Vec<Tool>,
}

/// Build a stage's [`StageSetup`] from its blueprint definition: inference config
/// (from the model parameters), tool-result routing, accepts-messages, layout,
/// and system prompt.
fn stage_setup_from(stage: &leviath_core::Stage) -> StageSetup {
    let temperature = stage
        .model
        .parameters
        .get("temperature")
        .and_then(|v| v.as_f64())
        .map(|t| t as f32);
    let max_output_tokens = stage
        .model
        .parameters
        .get("max_output_tokens")
        .and_then(|v| v.as_u64())
        .map(|t| t as usize);
    let base_prompt = stage
        .config
        .get("system_prompt")
        .and_then(|v| v.as_str())
        .map(String::from);
    // A fan-out stage's single inference IS the "split": fold its `split_prompt`
    // (which asks for the JSON array of work items) onto any base instructions so
    // the stage's normal inference produces the work items the split system parses.
    let system_prompt = match &stage.mode {
        leviath_core::blueprint::StageMode::FanOut { config }
            if !config.split_prompt.trim().is_empty() =>
        {
            Some(match base_prompt {
                Some(base) => format!("{base}\n\n{}", config.split_prompt),
                None => config.split_prompt.clone(),
            })
        }
        _ => base_prompt,
    };
    StageSetup {
        inference_config: InferenceConfig {
            temperature,
            max_output_tokens,
        },
        routing: stage.tool_result_routing.clone(),
        accepts_messages: stage.accepts_messages,
        context_layout: stage.context_layout.clone(),
        system_prompt,
    }
}

/// Spawn a fully-formed agent into `world` from its blueprint, task, and
/// per-stage resolution, and return its entity. Builds every stage's
/// `StageInference`/`StageSetup` up front (so transitions are pure component
/// swaps), seeds the context window, applies the **first** stage's setup (its
/// layout and system prompt), pre-counts the first stage's visit, and marks the
/// agent `ReadyToInfer`. Returns `Err` if the first stage's system prompt doesn't fit
/// its region (the same hard failure the imperative loop raises at stage 0).
///
/// `stages` must be aligned with `blueprint.stages` (one [`ResolvedStage`] each).
pub fn spawn_agent(
    world: &mut World,
    agent_id: String,
    blueprint: leviath_core::Blueprint,
    task: &str,
    stages: Vec<ResolvedStage>,
) -> Result<Entity, String> {
    let stage_infs: Vec<StageInference> = stages
        .into_iter()
        .map(|rs| StageInference {
            provider_name: rs.provider_name,
            model: rs.model,
            tools: rs.tools,
            tool_filter: None, // tools already resolved to the effective set
        })
        .collect();
    let setups: Vec<StageSetup> = blueprint.stages.iter().map(stage_setup_from).collect();

    // Seed the window from the blueprint layout + task, then apply stage 0's
    // context setup (layout swap + system-prompt injection) just as entering any
    // later stage would.
    let mut window = ContextWindow::new(blueprint.context_layout.total_budget_tokens);
    crate::context_setup::init_window(&mut window, &blueprint, task);
    apply_stage_context(&setups[0], &mut window)?;

    let stage0_name = blueprint.stages[0].name.clone();
    let stage0_inf = stage_infs[0].clone();
    let setup0 = &setups[0];
    let stage0_cfg = setup0.inference_config.clone();
    let stage0_routing = setup0.routing.clone();
    let accepts_messages = setup0.accepts_messages;

    // Pre-count stage 0's visit: the imperative loop bumps a stage's visit after
    // it runs and before resolving its transition, so stage 0 must read as
    // visited once by the time its first transition resolves.
    let mut visits = VisitCounts::default();
    *visits.0.entry(stage0_name.clone()).or_insert(0) += 1;

    // Seed the per-stage ledger (names + Pending) so the dashboard shows every
    // stage's real name from the first persist, not just the active one.
    let ledger = StageLedger(
        blueprint
            .stages
            .iter()
            .enumerate()
            .map(|(i, s)| leviath_core::run_meta::StageRecord::new(s.name.clone(), i))
            .collect(),
    );

    let entity = world
        .spawn((
            AgentBlueprint(blueprint),
            AgentState {
                agent_id,
                current_stage: stage0_name,
                iteration: 0,
                status: AgentStatus::Active,
                spawned_children_ids: vec![],
                pending_wait: None,
                accepts_messages,
            },
            MessageInbox::default(),
            StageCursor { index: 0 },
            StageProgress::default(),
            StageInferences(stage_infs),
            StageSetups(setups),
            visits,
            window,
            stage0_inf,
            stage0_cfg,
            ReadyToInfer,
        ))
        .id();
    // Inserted after spawn: the bundle above is already at bevy's 15-tuple limit.
    world
        .entity_mut(entity)
        .insert((ledger, StageIoBuffer::default()));
    if let Some(routing) = stage0_routing {
        world
            .entity_mut(entity)
            .insert(crate::components::ToolResultRoutingComponent { routing });
    }
    Ok(entity)
}

/// A transition-choice inference is in flight (an LLM is picking the next stage);
/// holds the choosable edges so the collect system can match the response back to
/// one. (Ported from the async portion of `graph::prompt_llm_transition`.)
#[derive(Component, Debug, Clone)]
pub struct AwaitingTransitionResponse(pub Vec<leviath_core::blueprint::TransitionEdge>);

/// The receiving end of the transition-choice outcomes channel, as a world
/// resource for the collect system. (The sending end lives in
/// [`InferenceStage::transition_outcomes`].)
#[derive(Resource)]
pub struct TransitionResults(pub UnboundedReceiver<InferenceOutcome>);

/// Build the LLM prompt that asks which stage to run next. (Ported from the
/// prompt-building portion of `graph::prompt_llm_transition`.)
fn build_transition_prompt(
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
/// or `None` if the stage may complete and the LLM said "DONE". Falls back to the
/// first edge when nothing matches. (Ported from the matching tail of
/// `graph::prompt_llm_transition`, keyed on `target` for pipeline consistency.)
fn match_transition_choice(
    choice: &str,
    edges: &[leviath_core::blueprint::TransitionEdge],
    allow_complete: bool,
) -> Option<String> {
    if allow_complete && choice.eq_ignore_ascii_case("done") {
        return None;
    }
    for edge in edges {
        if choice.eq_ignore_ascii_case(&edge.target) || choice.contains(edge.target.as_str()) {
            return Some(edge.target.clone());
        }
    }
    let lower = choice.to_lowercase();
    for edge in edges {
        if lower.contains(&edge.target.to_lowercase()) {
            return Some(edge.target.clone());
        }
    }
    // Nothing matched — fall back to the first available edge.
    edges.first().map(|edge| edge.target.clone())
}

/// Transition-choice dispatch: for each `AwaitingTransitionChoice` agent, inject
/// the "which stage next?" prompt into its context, build a short deterministic
/// request, acquire a per-model permit, spawn the inference onto the transition
/// lane, and move it to `AwaitingTransitionResponse`. Provider-missing / pool-full
/// leaves it choosing and retries next tick (same backpressure as
/// [`dispatch_inference`]).
#[allow(clippy::type_complexity)]
pub fn dispatch_transition_choice(
    mut agents: Query<
        (
            Entity,
            &AgentState,
            &mut ContextWindow,
            &StageInference,
            &AgentBlueprint,
            &StageCursor,
            &AwaitingTransitionChoice,
        ),
        With<AwaitingTransitionChoice>,
    >,
    stage: Res<InferenceStage>,
    providers: Res<Providers>,
    mut commands: Commands,
) {
    for (entity, state, mut window, si, bp, cursor, choice) in agents.iter_mut() {
        if state.status != AgentStatus::Active {
            continue; // paused / waiting / cancelled — don't start new work
        }
        let Some(provider) = providers.0.get(&si.provider_name).cloned() else {
            continue; // provider not registered — retry later
        };
        let Some(permit) = stage.pools.try_acquire(&si.model) else {
            continue; // pool full — retry next tick
        };

        let current = &bp.0.stages[cursor.index];
        let prompt = build_transition_prompt(current, &choice.0);
        let tokens = prompt.len() / 4 + 1;
        let _ = window.add_typed_entry(
            "conversation",
            leviath_core::EntryKind::UserMessage,
            prompt,
            tokens,
        );

        let assembled = window.assemble();
        let remaining = window.max_tokens.saturating_sub(window.current_tokens);
        let request = InferenceRequest {
            system: assembled.system_blocks,
            messages: assembled.messages,
            model: si.model.clone(),
            max_tokens: remaining.min(256), // short routing response
            temperature: 0.0,               // deterministic routing
            tools: Vec::new(),
            extra: serde_json::Value::Null,
        };

        let job = InferenceJob {
            entity,
            provider,
            request,
            permit,
        };
        stage.runtime.spawn(run_inference_job(
            job,
            stage.transition_outcomes.clone(),
            stage.wake.clone(),
        ));
        commands
            .entity(entity)
            .remove::<AwaitingTransitionChoice>()
            .insert(AwaitingTransitionResponse(choice.0.clone()));
    }
}

/// Transition-choice collect: drain completed routing inferences, match each to a
/// target stage (or completion), record the decision in context, and either enter
/// the chosen stage (loop to `ReadyToInfer`) or mark the agent `Complete`. A
/// provider error marks the agent `Error`.
#[allow(clippy::type_complexity)]
pub fn collect_transition_choice(
    mut results: ResMut<TransitionResults>,
    mut agents: Query<(
        &AgentBlueprint,
        &mut StageCursor,
        &mut AgentState,
        &mut StageProgress,
        &StageInferences,
        &StageSetups,
        &mut VisitCounts,
        &mut ContextWindow,
        &AwaitingTransitionResponse,
    )>,
    mut commands: Commands,
) {
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
        )) = agents.get_mut(outcome.entity)
        else {
            continue; // stale: agent cancelled/despawned since dispatch
        };
        let response = match outcome.result {
            Ok(response) => response,
            Err(err) => {
                state.status = AgentStatus::Error {
                    message: err.to_string(),
                };
                commands
                    .entity(outcome.entity)
                    .remove::<AwaitingTransitionResponse>();
                continue;
            }
        };

        let choice = response.content.trim().to_string();
        let tokens = choice.len() / 4 + 1;
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
                // Apply the chosen edge's context transform (Direct when the
                // matched target has no explicit edge, e.g. a fallback).
                let transform = resp
                    .0
                    .iter()
                    .find(|e| e.target == target)
                    .map(|e| e.transform.clone())
                    .unwrap_or_default();
                let to_compact = apply_edge_transform(&mut window, &transform);
                let setup = &setups.0[idx];
                match enter_stage(
                    idx,
                    &bp.0,
                    &mut cursor,
                    &mut state,
                    &mut progress,
                    &mut visits,
                    setup,
                    &mut window,
                ) {
                    Ok(()) => {
                        let name = bp.0.stages[idx].name.clone();
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
                        state.status = AgentStatus::Error { message };
                        commands
                            .entity(outcome.entity)
                            .remove::<AwaitingTransitionResponse>();
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

/// Notify the [`ToolService`] of every agent that just entered a stage (tagged
/// with [`StageJustEntered`] by the transition systems), so it can re-sync that
/// agent's per-stage tool permissions, then clear the tag. Runs after the
/// transition systems each tick.
pub fn sync_tool_stages(
    service: Res<ToolServiceRes>,
    entered: Query<(Entity, &StageJustEntered)>,
    mut commands: Commands,
) {
    for (entity, stage) in entered.iter() {
        service.0.sync_stage(entity, stage.index, &stage.name);
        commands.entity(entity).remove::<StageJustEntered>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference_pool::{InferencePoolConfig, InferencePools};
    use leviath_core::{Region, RegionKind};
    use tokio::sync::mpsc;

    /// A provider whose capabilities can be toggled for the temperature branch.
    struct Cfg {
        supports_temperature: bool,
        max_output: usize,
    }
    #[async_trait::async_trait]
    impl Provider for Cfg {
        async fn infer(
            &self,
            _r: InferenceRequest,
        ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
            Ok(leviath_providers::InferenceResponse {
                content: "ok".to_string(),
                tool_calls: vec![],
                tokens_used: leviath_providers::TokenUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                finish_reason: leviath_providers::FinishReason::Complete,
            })
        }
        fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "cfg"
        }
        fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities {
                supports_temperature: self.supports_temperature,
                max_output_tokens: self.max_output,
                ..Default::default()
            }
        }
    }

    fn window() -> ContextWindow {
        let mut w = ContextWindow::new(10_000);
        w.add_region(Region::new("sys".to_string(), RegionKind::Pinned, 1000));
        w
    }

    fn tool(name: &str) -> Tool {
        Tool {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::Value::Null,
        }
    }

    fn stage(model: &str, tools: Vec<Tool>, filter: Option<Vec<String>>) -> StageInference {
        StageInference {
            provider_name: "cfg".to_string(),
            model: model.to_string(),
            tools,
            tool_filter: filter,
        }
    }

    fn provider(supports_temperature: bool, max_output: usize) -> Arc<dyn Provider> {
        Arc::new(Cfg {
            supports_temperature,
            max_output,
        })
    }

    // ── build_request branch coverage ──

    #[test]
    fn build_request_filters_tools_and_uses_config_overrides() {
        let cfg = InferenceConfig {
            temperature: Some(0.1),
            max_output_tokens: Some(42),
        };
        let si = stage(
            "m",
            vec![tool("keep"), tool("drop")],
            Some(vec!["keep".into()]),
        );
        let req = build_request(&window(), Some(&cfg), &si, &provider(true, 9999));
        assert_eq!(req.tools.len(), 1); // filtered to "keep"
        assert_eq!(req.tools[0].name, "keep");
        assert_eq!(req.max_tokens, 42); // config output cap wins
        assert_eq!(req.temperature, 0.1); // config temperature
    }

    #[test]
    fn build_request_all_tools_default_temperature_no_config() {
        let si = stage("m", vec![tool("a"), tool("b")], None); // None filter = all
        let req = build_request(&window(), None, &si, &provider(true, 500));
        assert_eq!(req.tools.len(), 2);
        assert_eq!(req.temperature, 0.7); // default when supported and no config
        assert_eq!(req.max_tokens, 500); // capability cap when no config override
    }

    #[test]
    fn build_request_empty_filter_is_all_and_no_temperature_when_unsupported() {
        let si = stage("m", vec![tool("a")], Some(vec![])); // empty filter = all
        let req = build_request(&window(), None, &si, &provider(false, 500));
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.temperature, 0.0); // model doesn't support temperature
    }

    #[test]
    fn cfg_provider_metadata_is_exercised() {
        // Keep the mock's non-`infer`/`capabilities` trait methods measured.
        let p = Cfg {
            supports_temperature: true,
            max_output: 1,
        };
        assert_eq!(p.name(), "cfg");
        assert_eq!(p.count_tokens("t", "m"), 1);
        assert_eq!(p.max_context_tokens("m"), 100_000);
    }

    // ── dispatch system ──

    fn build_world(pools: InferencePools) -> (World, mpsc::UnboundedReceiver<InferenceOutcome>) {
        let mut registry = ProviderRegistry::new();
        registry.register("cfg".to_string(), provider(true, 1000));
        let (tx, rx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(Providers(registry));
        let (ttx, _trx) = mpsc::unbounded_channel();
        let (ctx, _crx) = mpsc::unbounded_channel();
        world.insert_resource(InferenceStage {
            pools: Arc::new(pools),
            outcomes: tx,
            transition_outcomes: ttx,
            compaction_outcomes: ctx,
            wake: Arc::new(Notify::new()),
            runtime: Handle::current(),
        });
        (world, rx)
    }

    fn run(world: &mut World) {
        let mut schedule = Schedule::default();
        schedule.add_systems(dispatch_inference);
        schedule.run(world);
    }

    #[tokio::test]
    async fn dispatch_moves_agent_to_awaiting_and_runs_the_job() {
        let (mut world, mut rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let e = world
            .spawn((
                agent_state(),
                window(),
                stage("m", vec![], None),
                ReadyToInfer,
            ))
            .id();

        run(&mut world);

        // Phase advanced.
        assert!(world.get::<AwaitingInference>(e).is_some());
        assert!(world.get::<ReadyToInfer>(e).is_none());
        // The spawned job ran and reported an outcome.
        let outcome = rx.recv().await.expect("outcome");
        assert_eq!(outcome.entity, e);
        assert!(outcome.result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_skips_when_pool_full() {
        let mut cfg = InferencePoolConfig::new();
        cfg.set_limit("m", 1);
        let pools = InferencePools::new(cfg);
        let _held = pools.try_acquire("m").unwrap(); // occupy the only slot
        let (mut world, _rx) = build_world(pools);
        let e = world
            .spawn((
                agent_state(),
                window(),
                stage("m", vec![], None),
                ReadyToInfer,
            ))
            .id();

        run(&mut world);

        // No slot ⇒ still ready, not dispatched.
        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<AwaitingInference>(e).is_none());
    }

    #[tokio::test]
    async fn dispatch_skips_when_provider_missing() {
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let e = world
            .spawn((
                agent_state(),
                window(),
                stage("m", vec![], None).clone_with_provider("nope"),
                ReadyToInfer,
            ))
            .id();

        run(&mut world);

        assert!(world.get::<ReadyToInfer>(e).is_some()); // unknown provider ⇒ untouched
        assert!(world.get::<AwaitingInference>(e).is_none());
    }

    #[tokio::test]
    async fn dispatch_inference_skips_non_active_agent() {
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let mut st = agent_state();
        st.status = AgentStatus::Idle; // paused
        let e = world
            .spawn((st, window(), stage("m", vec![], None), ReadyToInfer))
            .id();

        run(&mut world);

        // Paused ⇒ not dispatched, stays ready for when it resumes.
        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<AwaitingInference>(e).is_none());
    }

    impl StageInference {
        fn clone_with_provider(mut self, name: &str) -> Self {
            self.provider_name = name.to_string();
            self
        }
    }

    // ── collect system ──

    fn agent_state() -> AgentState {
        AgentState {
            agent_id: "a".to_string(),
            current_stage: "s".to_string(),
            iteration: 0,
            status: AgentStatus::Active,
            spawned_children_ids: vec![],
            pending_wait: None,
            accepts_messages: true,
        }
    }

    fn resp(text: &str) -> leviath_providers::InferenceResponse {
        leviath_providers::InferenceResponse {
            content: text.to_string(),
            tool_calls: vec![],
            tokens_used: leviath_providers::TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            finish_reason: leviath_providers::FinishReason::Complete,
        }
    }

    fn run_collect(world: &mut World) {
        let mut schedule = Schedule::default();
        schedule.add_systems(collect_inference);
        schedule.run(world);
    }

    fn world_with_results() -> (World, mpsc::UnboundedSender<InferenceOutcome>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(InferenceResults(rx));
        (world, tx)
    }

    #[test]
    fn collect_applies_ok_and_advances_to_process_response() {
        let (mut world, tx) = world_with_results();
        let e = world.spawn((agent_state(), AwaitingInference)).id();
        let mut response = resp("hi");
        response.tool_calls.push(leviath_providers::ToolCall {
            id: "call-1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "x"}),
        });
        tx.send(InferenceOutcome {
            entity: e,
            result: Ok(response),
        })
        .unwrap();

        run_collect(&mut world);

        assert!(world.get::<ProcessResponse>(e).is_some());
        assert!(world.get::<AwaitingInference>(e).is_none());
        assert_eq!(world.get::<AgentState>(e).unwrap().iteration, 1);
        let stored = world.get::<crate::components::InferenceResult>(e).unwrap();
        assert_eq!(stored.response, "hi");
        // The tool call was mapped onto the stored result.
        assert_eq!(stored.tool_calls.len(), 1);
        assert_eq!(stored.tool_calls[0].name, "read_file");
    }

    #[test]
    fn collect_marks_error_on_failure() {
        let (mut world, tx) = world_with_results();
        let e = world.spawn((agent_state(), AwaitingInference)).id();
        tx.send(InferenceOutcome {
            entity: e,
            result: Err(leviath_providers::ProviderError::Other("boom".to_string())),
        })
        .unwrap();

        run_collect(&mut world);

        // `ProviderError::Other`'s Display is the inner message ("boom").
        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Error {
                message: "boom".to_string()
            }
        );
        assert!(world.get::<AwaitingInference>(e).is_none());
        // The error is routed to the transition logic (which follows an `error`
        // edge if the stage has one, else terminates).
        assert!(world.get::<ResolveTransition>(e).is_some());
        assert_eq!(
            world.get::<StageOutcome>(e).unwrap(),
            &StageOutcome::Errored("boom".to_string())
        );
    }

    // ── stage-io persistence (#1) ──

    fn ledger2() -> StageLedger {
        StageLedger(vec![
            leviath_core::run_meta::StageRecord::new("plan".to_string(), 0),
            leviath_core::run_meta::StageRecord::new("impl".to_string(), 1),
        ])
    }

    #[test]
    fn one_line_collapses_whitespace_and_truncates() {
        assert_eq!(one_line("a\n  b\tc ", 100), "a b c");
        let long = "x".repeat(250);
        let out = one_line(&long, 200);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 201); // 200 chars + the ellipsis
    }

    #[test]
    fn reconcile_stage_ledger_sets_past_active_future_once() {
        use leviath_core::run_meta::StageRunStatus;
        let mut led = StageLedger(vec![
            leviath_core::run_meta::StageRecord::new("a".to_string(), 0),
            leviath_core::run_meta::StageRecord::new("b".to_string(), 1),
            leviath_core::run_meta::StageRecord::new("c".to_string(), 2),
        ]);
        reconcile_stage_ledger(&mut led, 1, &AgentStatus::Active, 100);
        assert_eq!(led.0[0].status, StageRunStatus::Complete);
        assert_eq!(led.0[0].started_at, Some(100));
        assert_eq!(led.0[0].ended_at, Some(100));
        assert_eq!(led.0[1].status, StageRunStatus::Active);
        assert_eq!(led.0[1].started_at, Some(100));
        assert_eq!(led.0[1].ended_at, None);
        assert_eq!(led.0[2].status, StageRunStatus::Pending);

        // Idempotent: a later reconcile doesn't overwrite the stamped timestamps.
        reconcile_stage_ledger(&mut led, 1, &AgentStatus::Active, 200);
        assert_eq!(led.0[0].ended_at, Some(100));
        assert_eq!(led.0[1].started_at, Some(100));
    }

    #[test]
    fn reconcile_stage_ledger_completes_current_stage_on_run_complete() {
        use leviath_core::run_meta::StageRunStatus;
        let mut led = StageLedger(vec![leviath_core::run_meta::StageRecord::new(
            "a".to_string(),
            0,
        )]);
        reconcile_stage_ledger(&mut led, 0, &AgentStatus::Complete, 50);
        assert_eq!(led.0[0].status, StageRunStatus::Complete);
        assert_eq!(led.0[0].ended_at, Some(50));
    }

    #[test]
    fn collect_inference_buffers_output_token_line_and_stage_tokens() {
        let (mut world, tx) = world_with_results();
        let e = world
            .spawn((
                agent_state(),
                AwaitingInference,
                StageCursor { index: 1 },
                ledger2(),
                StageIoBuffer::default(),
            ))
            .id();
        let mut response = resp("the plan");
        response.tokens_used.prompt_tokens = 5;
        response.tokens_used.completion_tokens = 3;
        response.tokens_used.cached_tokens = 2;
        tx.send(InferenceOutcome {
            entity: e,
            result: Ok(response),
        })
        .unwrap();

        run_collect(&mut world);

        let buf = world.get::<StageIoBuffer>(e).unwrap();
        assert_eq!(buf.output, vec![(1, "the plan".to_string())]);
        assert_eq!(buf.logs, vec![(1, "[Tokens: 5 in, 3 out]".to_string())]);
        let led = world.get::<StageLedger>(e).unwrap();
        assert_eq!(led.0[1].prompt_tokens, 5);
        assert_eq!(led.0[1].completion_tokens, 3);
        assert_eq!(led.0[1].cached_tokens, 2);
    }

    #[test]
    fn collect_inference_skips_empty_output_but_logs_tokens() {
        let (mut world, tx) = world_with_results();
        let e = world
            .spawn((
                agent_state(),
                AwaitingInference,
                StageCursor { index: 0 },
                StageIoBuffer::default(),
            ))
            .id();
        tx.send(InferenceOutcome {
            entity: e,
            result: Ok(resp("   ")), // whitespace-only ⇒ no output line
        })
        .unwrap();

        run_collect(&mut world);

        let buf = world.get::<StageIoBuffer>(e).unwrap();
        assert!(buf.output.is_empty());
        assert_eq!(buf.logs.len(), 1); // token line only
    }

    #[test]
    fn collect_inference_error_buffers_error_line() {
        let (mut world, tx) = world_with_results();
        let e = world
            .spawn((
                agent_state(),
                AwaitingInference,
                StageCursor { index: 0 },
                StageIoBuffer::default(),
            ))
            .id();
        tx.send(InferenceOutcome {
            entity: e,
            result: Err(leviath_providers::ProviderError::Other("boom".to_string())),
        })
        .unwrap();

        run_collect(&mut world);

        let buf = world.get::<StageIoBuffer>(e).unwrap();
        assert_eq!(buf.logs, vec![(0, "[error] boom".to_string())]);
    }

    #[test]
    fn collect_inference_tolerates_cursor_beyond_ledger() {
        let (mut world, tx) = world_with_results();
        let e = world
            .spawn((
                agent_state(),
                AwaitingInference,
                StageCursor { index: 9 }, // past the 2-stage ledger
                ledger2(),
                StageIoBuffer::default(),
            ))
            .id();
        tx.send(InferenceOutcome {
            entity: e,
            result: Ok(resp("x")),
        })
        .unwrap();

        run_collect(&mut world);

        // No panic; output tagged with idx 9, ledger tokens untouched.
        assert_eq!(
            world.get::<StageIoBuffer>(e).unwrap().output,
            vec![(9, "x".to_string())]
        );
        assert_eq!(world.get::<StageLedger>(e).unwrap().0[0].prompt_tokens, 0);
    }

    #[test]
    fn collect_tools_buffers_one_tool_log_line_per_call() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(ToolResults(rx));
        let e = world
            .spawn((
                ctx(&[("conversation", 10_000)]),
                infer_with(vec![tc("c1", "read_file")]),
                AwaitingTools,
                StageCursor { index: 2 },
                StageIoBuffer::default(),
            ))
            .id();
        tx.send(ToolOutcome {
            entity: e,
            results: vec![("c1".to_string(), "file\nbody".to_string())],
        })
        .unwrap();

        run_collect_tools(&mut world);

        let buf = world.get::<StageIoBuffer>(e).unwrap();
        assert_eq!(
            buf.logs,
            vec![(2, "[tool] read_file: file body".to_string())]
        );
    }

    #[test]
    fn dispatch_persistence_emits_stage_index_and_drains_io_buffer() {
        use leviath_core::run_meta::StageRunStatus;
        let (mut world, mut rx) = world_with_persistence();
        let mut buf = StageIoBuffer::default();
        buf.output.push((0, "hello".to_string()));
        buf.logs.push((0, "[tool] x: y".to_string()));
        let e = world
            .spawn((
                run_metadata(),
                agent_state(),
                conv_window(),
                StageCursor { index: 0 },
                TokenTotals::default(),
                PersistWatermark::default(),
                ledger2(),
                buf,
            ))
            .id();

        run_dispatch_persistence(&mut world);

        let job = rx.try_recv().expect("job sent");
        assert_eq!(job.stages.len(), 2);
        assert_eq!(job.stages[0].name, "plan");
        assert_eq!(job.stages[0].status, StageRunStatus::Active);
        assert_eq!(job.output_appends, vec![(0, "hello".to_string())]);
        assert_eq!(job.log_appends, vec![(0, "[tool] x: y".to_string())]);
        // The buffer was drained in place.
        assert!(world.get::<StageIoBuffer>(e).unwrap().output.is_empty());
    }

    #[test]
    fn dispatch_persistence_flushes_buffered_io_without_a_watermark_change() {
        let (mut world, mut rx) = world_with_persistence();
        let e = world
            .spawn((
                run_metadata(),
                agent_state(),
                conv_window(),
                StageCursor { index: 0 },
                TokenTotals::default(),
                PersistWatermark::default(),
                StageIoBuffer::default(),
            ))
            .id();

        // First pass: watermark changes ⇒ a job is sent, buffer stays empty.
        run_dispatch_persistence(&mut world);
        let _ = rx.try_recv().expect("first job");

        // Watermark unchanged, but new buffered content ⇒ still flushed.
        world
            .get_mut::<StageIoBuffer>(e)
            .unwrap()
            .logs
            .push((0, "late log".to_string()));
        run_dispatch_persistence(&mut world);
        let job = rx.try_recv().expect("append-triggered job");
        assert_eq!(job.log_appends, vec![(0, "late log".to_string())]);
    }

    #[test]
    fn spawn_agent_seeds_the_stage_ledger_with_names() {
        let mk = |name: &str| {
            leviath_core::Stage::new(
                name.to_string(),
                leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
            )
        };
        let bp = blueprint(vec![mk("plan"), mk("build")]);
        let mut world = World::new();
        let e = spawn_agent(
            &mut world,
            "run-led".to_string(),
            bp,
            "task",
            vec![resolved("m"), resolved("m")],
        )
        .expect("spawn");
        let led = world.get::<StageLedger>(e).expect("ledger seeded");
        assert_eq!(led.0.len(), 2);
        assert_eq!(led.0[0].name, "plan");
        assert_eq!(led.0[1].name, "build");
        assert!(world.get::<StageIoBuffer>(e).is_some());
    }

    #[test]
    fn collect_drops_outcome_for_non_awaiting_agent() {
        let (mut world, tx) = world_with_results();
        let e = world.spawn(agent_state()).id(); // no AwaitingInference marker
        tx.send(InferenceOutcome {
            entity: e,
            result: Ok(resp("x")),
        })
        .unwrap();

        run_collect(&mut world);

        // Untouched — the stale outcome was dropped.
        assert_eq!(world.get::<AgentState>(e).unwrap().iteration, 0);
        assert!(world.get::<ProcessResponse>(e).is_none());
    }

    #[test]
    fn collect_inference_accumulates_token_totals() {
        let (mut world, tx) = world_with_results();
        let e = world
            .spawn((
                agent_state(),
                AwaitingInference,
                crate::persistence::TokenTotals::default(),
            ))
            .id();
        let mut r = resp("hi");
        r.tokens_used = leviath_providers::TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cached_tokens: 2,
            cache_write_tokens: 1,
        };
        tx.send(InferenceOutcome {
            entity: e,
            result: Ok(r),
        })
        .unwrap();

        run_collect(&mut world);

        let t = world.get::<crate::persistence::TokenTotals>(e).unwrap();
        assert_eq!(t.prompt_tokens, 10);
        assert_eq!(t.completion_tokens, 5);
        assert_eq!(t.cached_tokens, 2);
        assert_eq!(t.cache_write_tokens, 1);
    }

    // ── process-response routing ──

    fn infer_result(with_tools: bool) -> crate::components::InferenceResult {
        crate::components::InferenceResult {
            response: "r".to_string(),
            tool_calls: if with_tools {
                vec![crate::components::ToolCall {
                    tool_id: "t".to_string(),
                    name: "n".to_string(),
                    arguments: serde_json::Value::Null,
                }]
            } else {
                vec![]
            },
            tokens_used: 0,
            timestamp: 0,
        }
    }

    fn run_process(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(process_response);
        s.run(world);
    }

    #[test]
    fn process_routes_tool_calls_to_ready_for_tools() {
        let mut world = World::new();
        let e = world
            .spawn((
                infer_result(true),
                StageProgress::default(),
                ProcessResponse,
            ))
            .id();
        run_process(&mut world);
        assert!(world.get::<ReadyForTools>(e).is_some());
        assert!(world.get::<ProcessResponse>(e).is_none());
        assert!(world.get::<ReadyForTransition>(e).is_none());
        // The stage's running tool-call count was bumped.
        assert_eq!(world.get::<StageProgress>(e).unwrap().total_tool_calls, 1);
    }

    #[test]
    fn process_response_bumps_tool_calls_in_token_totals() {
        let mut world = World::new();
        let e = world
            .spawn((
                infer_result(true),
                StageProgress::default(),
                crate::persistence::TokenTotals::default(),
                ProcessResponse,
            ))
            .id();
        run_process(&mut world);
        assert_eq!(
            world
                .get::<crate::persistence::TokenTotals>(e)
                .unwrap()
                .tool_calls,
            1
        );
    }

    #[test]
    fn process_routes_no_tools_to_ready_for_transition() {
        let mut world = World::new();
        let e = world
            .spawn((
                infer_result(false),
                StageProgress::default(),
                ProcessResponse,
            ))
            .id();
        run_process(&mut world);
        assert!(world.get::<ReadyForTransition>(e).is_some());
        assert!(world.get::<ReadyForTools>(e).is_none());
    }

    // ── empty-response (finish vs. nudge) ──

    fn run_empty(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(handle_empty_response);
        s.run(world);
    }

    #[test]
    fn empty_response_finishes_when_agent_made_tool_calls() {
        let mut world = World::new();
        let progress = StageProgress {
            total_tool_calls: 2,
            text_only_nudges: 0,
            iterations: 0,
        };
        let e = world
            .spawn((
                ctx(&[("conversation", 10_000)]),
                infer_result(false),
                progress,
                ReadyForTransition,
            ))
            .id();
        run_empty(&mut world);
        assert!(world.get::<ResolveTransition>(e).is_some());
        assert!(world.get::<ReadyForTransition>(e).is_none());
    }

    #[test]
    fn empty_response_finishes_after_max_nudges() {
        let mut world = World::new();
        let progress = StageProgress {
            total_tool_calls: 0,
            text_only_nudges: MAX_TEXT_ONLY_NUDGES,
            iterations: 0,
        };
        let e = world
            .spawn((
                ctx(&[("conversation", 10_000)]),
                infer_result(false),
                progress,
                ReadyForTransition,
            ))
            .id();
        run_empty(&mut world);
        assert!(world.get::<ResolveTransition>(e).is_some());
    }

    #[test]
    fn empty_response_nudges_and_loops_back_when_text_only() {
        let mut world = World::new();
        let e = world
            .spawn((
                ctx(&[("conversation", 10_000)]),
                infer_result(false),
                StageProgress::default(),
                ReadyForTransition,
            ))
            .id();
        run_empty(&mut world);
        // Nudged: back to infer, counter bumped, nudge added to context.
        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<ResolveTransition>(e).is_none());
        assert_eq!(world.get::<StageProgress>(e).unwrap().text_only_nudges, 1);
        assert!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .current_tokens
                > 0
        );
    }

    // ── tool-dispatch ──

    /// A tool service that echoes each call as `(id, "ran <name>")`.
    struct EchoService;
    impl ToolService for EchoService {
        fn exec_for(
            &self,
            _entity: Entity,
            calls: Vec<leviath_providers::ToolCall>,
        ) -> BoxedToolExec {
            Box::new(move || {
                Box::pin(async move {
                    calls
                        .into_iter()
                        .map(|c| (c.id, format!("ran {}", c.name)))
                        .collect()
                })
            })
        }
    }

    /// A tool service that records every `sync_stage` call.
    #[derive(Default)]
    struct RecordingService(Arc<std::sync::Mutex<Vec<(Entity, usize, String)>>>);
    impl ToolService for RecordingService {
        fn exec_for(
            &self,
            _entity: Entity,
            _calls: Vec<leviath_providers::ToolCall>,
        ) -> BoxedToolExec {
            Box::new(|| Box::pin(async { Vec::new() }))
        }
        fn sync_stage(&self, entity: Entity, stage_index: usize, stage_name: &str) {
            self.0
                .lock()
                .unwrap()
                .push((entity, stage_index, stage_name.to_string()));
        }
    }

    #[tokio::test]
    async fn sync_tool_stages_notifies_service_and_clears_marker() {
        let mut world = World::new();
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let service = Arc::new(RecordingService(log.clone()));
        world.insert_resource(ToolServiceRes(service.clone()));
        let entity = world
            .spawn(StageJustEntered {
                index: 2,
                name: "review".to_string(),
            })
            .id();
        let mut schedule = Schedule::default();
        schedule.add_systems(sync_tool_stages);
        schedule.run(&mut world);

        assert_eq!(
            log.lock().unwrap().as_slice(),
            &[(entity, 2, "review".to_string())]
        );
        // The transient marker is cleared after notifying.
        assert!(world.get::<StageJustEntered>(entity).is_none());
        // The service's tool executor still runs (returns no results here).
        assert!(service.exec_for(entity, Vec::new())().await.is_empty());
    }

    #[test]
    fn default_sync_stage_is_a_noop() {
        // A service that doesn't override `sync_stage` uses the no-op default.
        EchoService.sync_stage(Entity::from_raw(0), 3, "x");
    }

    #[tokio::test]
    async fn dispatch_tools_enqueues_runnable_job_and_advances() {
        let (jtx, mut jrx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
        world.insert_resource(ToolStage(jtx));
        let e = world
            .spawn((
                agent_state(),
                infer_result(true),
                conv_window(),
                ReadyForTools,
            ))
            .id();

        let mut s = Schedule::default();
        s.add_systems(dispatch_tools);
        s.run(&mut world);

        assert!(world.get::<AwaitingTools>(e).is_some());
        assert!(world.get::<ReadyForTools>(e).is_none());
        let job = jrx.try_recv().expect("job enqueued");
        assert_eq!(job.entity, e);
        // Run the produced closure (covers the service's exec path).
        let results = (job.exec)().await;
        assert_eq!(results, vec![("t".to_string(), "ran n".to_string())]);
    }

    #[tokio::test]
    async fn dispatch_tools_skips_non_active_agent() {
        let (jtx, mut jrx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
        world.insert_resource(ToolStage(jtx));
        let mut st = agent_state();
        st.status = AgentStatus::Cancelled;
        let e = world
            .spawn((st, infer_result(true), conv_window(), ReadyForTools))
            .id();

        let mut s = Schedule::default();
        s.add_systems(dispatch_tools);
        s.run(&mut world);

        assert!(world.get::<ReadyForTools>(e).is_some()); // cancelled ⇒ not enqueued
        assert!(jrx.try_recv().is_err());
    }

    fn infer_with(calls: Vec<crate::components::ToolCall>) -> crate::components::InferenceResult {
        crate::components::InferenceResult {
            response: "r".to_string(),
            tool_calls: calls,
            tokens_used: 0,
            timestamp: 0,
        }
    }

    fn ctx_call(id: &str, region: &str, content: &str) -> crate::components::ToolCall {
        crate::components::ToolCall {
            tool_id: id.to_string(),
            name: "context_write".to_string(),
            arguments: serde_json::json!({"region": region, "content": content}),
        }
    }

    fn notes_window() -> ContextWindow {
        let mut w = conv_window();
        w.add_region(Region::new(
            "notes".to_string(),
            RegionKind::Clearable,
            5000,
        ));
        w
    }

    #[tokio::test]
    async fn dispatch_tools_applies_all_context_inline() {
        let (jtx, mut jrx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
        world.insert_resource(ToolStage(jtx));
        let e = world
            .spawn((
                agent_state(),
                infer_with(vec![ctx_call("c1", "notes", "hi")]),
                notes_window(),
                ReadyForTools,
            ))
            .id();

        let mut s = Schedule::default();
        s.add_systems(dispatch_tools);
        s.run(&mut world);

        // All-context batch: nothing enqueued, applied inline, ready to infer.
        assert!(jrx.try_recv().is_err());
        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<ReadyForTools>(e).is_none());
        assert!(world.get::<ContextToolResults>(e).is_none());
        assert!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("notes")
                .unwrap()
                .current_tokens
                > 0
        );
    }

    #[tokio::test]
    async fn dispatch_tools_partitions_context_and_lane() {
        let (jtx, mut jrx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
        world.insert_resource(ToolStage(jtx));
        let e = world
            .spawn((
                agent_state(),
                infer_with(vec![ctx_call("c1", "notes", "hi"), tc("c2", "read_file")]),
                notes_window(),
                ReadyForTools,
            ))
            .id();

        let mut s = Schedule::default();
        s.add_systems(dispatch_tools);
        s.run(&mut world);

        // Context result stashed; the non-context call went to the lane.
        assert!(world.get::<AwaitingTools>(e).is_some());
        let stashed = world.get::<ContextToolResults>(e).unwrap();
        assert_eq!(stashed.0.len(), 1);
        assert_eq!(stashed.0[0].0, "c1");
        let job = jrx.try_recv().expect("lane job for the non-context call");
        assert_eq!(job.entity, e);
    }

    // ── taint gate (dispatch_tools) ──

    /// A taint-tracking window carrying `Internal`-level data.
    fn tainted_conv_window() -> ContextWindow {
        let mut w = conv_window();
        w.enable_taint_tracking();
        let _ = w.add_typed_tainted_to_region(
            "conversation",
            leviath_core::EntryKind::UserMessage,
            "secret".to_string(),
            5,
            leviath_core::TaintLevel::Internal,
        );
        w
    }

    fn enabled_gate() -> crate::taint::TaintGate {
        crate::taint::TaintGate::new(leviath_core::SecurityConfig {
            taint_tracking: true,
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn dispatch_tools_gate_blocks_outbound_leak_but_allows_inbound() {
        let (jtx, mut jrx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
        world.insert_resource(ToolStage(jtx));
        // `shell` is outbound (clearance Public) over Internal data ⇒ blocked;
        // `read_file` is inbound ⇒ always allowed ⇒ goes to the lane.
        let e = world
            .spawn((
                agent_state(),
                infer_with(vec![tc("c_shell", "shell"), tc("c_read", "read_file")]),
                tainted_conv_window(),
                ReadyForTools,
                enabled_gate(),
            ))
            .id();
        let mut s = Schedule::default();
        s.add_systems(dispatch_tools);
        s.run(&mut world);

        assert!(world.get::<AwaitingTools>(e).is_some());
        let stashed = world.get::<ContextToolResults>(e).unwrap();
        assert!(
            stashed
                .0
                .iter()
                .any(|(id, msg)| id == "c_shell" && msg.contains("[blocked]"))
        );
        let job = jrx.try_recv().expect("read_file enqueued to the lane");
        assert_eq!(job.entity, e);
    }

    #[tokio::test]
    async fn dispatch_tools_gate_allows_outbound_via_allowlist() {
        let (jtx, mut jrx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
        world.insert_resource(ToolStage(jtx));
        // An allowlist rule permits `shell` up to Internal sensitivity.
        world.insert_resource(PolicyGate(leviath_core::PolicyConfig {
            allowlist: vec![leviath_core::policy::AllowlistRule {
                tool: "shell".to_string(),
                to: vec![],
                channel: vec![],
                max_sensitivity: leviath_core::TaintLevel::Internal,
            }],
            mcp_overrides: Default::default(),
        }));
        let e = world
            .spawn((
                agent_state(),
                infer_with(vec![tc("c_shell", "shell")]),
                tainted_conv_window(),
                ReadyForTools,
                enabled_gate(),
            ))
            .id();
        let mut s = Schedule::default();
        s.add_systems(dispatch_tools);
        s.run(&mut world);

        // Allowlisted ⇒ the outbound call reaches the lane instead of `[blocked]`.
        assert!(world.get::<AwaitingTools>(e).is_some());
        let job = jrx.try_recv().expect("shell enqueued via allowlist");
        assert_eq!(job.entity, e);
    }

    #[tokio::test]
    async fn dispatch_tools_gate_allows_outbound_via_scripted_rule() {
        let (jtx, mut jrx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
        world.insert_resource(ToolStage(jtx));
        // No static allowlist, but a scripted rule that permits `shell`.
        let checker: std::sync::Arc<crate::taint::ScriptRuleChecker> =
            std::sync::Arc::new(|tool: &str, _target: Option<&str>, _taint| {
                (tool == "shell").then(|| "scripted".to_string())
            });
        world.insert_resource(GateScriptRules(checker));
        let e = world
            .spawn((
                agent_state(),
                infer_with(vec![tc("c_shell", "shell")]),
                tainted_conv_window(),
                ReadyForTools,
                enabled_gate(),
            ))
            .id();
        let mut s = Schedule::default();
        s.add_systems(dispatch_tools);
        s.run(&mut world);

        // The scripted rule allows it ⇒ reaches the lane, not `[blocked]`.
        assert!(world.get::<AwaitingTools>(e).is_some());
        let job = jrx.try_recv().expect("shell enqueued via scripted rule");
        assert_eq!(job.entity, e);
    }

    #[test]
    fn taint_block_message_renders_blocked_and_falls_back() {
        use leviath_core::taint::GateDecision;
        let blocked = GateDecision::Blocked {
            taint_level: leviath_core::TaintLevel::Internal,
            clearance: leviath_core::TaintLevel::Public,
            source_regions: vec!["conversation".to_string()],
            tool_name: "shell".to_string(),
        };
        let msg = taint_block_message(&blocked);
        assert!(msg.contains("shell") && msg.contains("conversation") && msg.contains("[blocked]"));
        // Empty source regions render as "context".
        let blocked_empty = GateDecision::Blocked {
            taint_level: leviath_core::TaintLevel::Internal,
            clearance: leviath_core::TaintLevel::Public,
            source_regions: vec![],
            tool_name: "shell".to_string(),
        };
        assert!(taint_block_message(&blocked_empty).contains("context"));
        // The Allowed arm is only a defensive fallback.
        assert!(taint_block_message(&GateDecision::Allowed).contains("blocked"));
    }

    #[test]
    fn merge_in_call_order_fills_missing_with_empty() {
        let calls = vec![tc("a", "x"), tc("b", "y")];
        // Only "a" has a result; "b" falls back to empty, in call order.
        let merged = merge_in_call_order(&calls, &[("a".to_string(), "ra".to_string())]);
        assert_eq!(
            merged,
            vec![
                ("a".to_string(), "ra".to_string()),
                ("b".to_string(), String::new()),
            ]
        );
    }

    // ── tool-collect (apply_tool_results) ──

    fn ctx(regions: &[(&str, usize)]) -> ContextWindow {
        let mut w = ContextWindow::new(100_000);
        for (name, max) in regions {
            w.add_region(Region::new(name.to_string(), RegionKind::Clearable, *max));
        }
        w
    }

    fn tc(id: &str, name: &str) -> crate::components::ToolCall {
        crate::components::ToolCall {
            tool_id: id.to_string(),
            name: name.to_string(),
            arguments: serde_json::Value::Null,
        }
    }

    fn routing(
        default: &str,
        overrides: &[(&str, &str)],
        persist: bool,
        max_result: Option<usize>,
    ) -> leviath_core::blueprint::ToolResultRouting {
        leviath_core::blueprint::ToolResultRouting {
            default_region: default.to_string(),
            tool_overrides: overrides
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            persist,
            max_result_tokens: max_result,
        }
    }

    #[test]
    fn apply_adds_assistant_turn_and_result_to_conversation() {
        let mut w = ctx(&[("conversation", 10_000)]);
        apply_tool_results(
            &mut w,
            "resp",
            &[tc("c1", "read")],
            &[("c1".to_string(), "result".to_string())],
            None,
            None,
        );
        assert!(w.get_region("conversation").unwrap().current_tokens > 0);
    }

    #[test]
    fn apply_falls_back_when_region_missing() {
        let mut w = ctx(&[]); // no "conversation" region — every add errors
        // Exhausts the forced-add fallback to the placeholder without panicking.
        apply_tool_results(
            &mut w,
            "resp",
            &[tc("c1", "read")],
            &[("c1".to_string(), "long result".to_string())],
            None,
            None,
        );
    }

    #[test]
    fn apply_routes_to_override_region() {
        let mut w = ctx(&[("conversation", 10_000), ("special", 10_000)]);
        let r = routing("conversation", &[("read", "special")], true, None);
        apply_tool_results(
            &mut w,
            "resp",
            &[tc("c1", "read")],
            &[("c1".to_string(), "x".to_string())],
            Some(&r),
            None,
        );
        assert!(w.get_region("special").unwrap().current_tokens > 0);
    }

    #[test]
    fn apply_default_region_when_no_override() {
        let mut w = ctx(&[("dflt", 10_000)]);
        let r = routing("dflt", &[], true, None); // no matching override for "read"
        apply_tool_results(
            &mut w,
            "resp",
            &[tc("c1", "read")],
            &[("c1".to_string(), "x".to_string())],
            Some(&r),
            None,
        );
        assert!(w.get_region("dflt").unwrap().current_tokens > 0);
    }

    #[test]
    fn apply_routes_to_scratch_when_not_persist() {
        let mut w = ctx(&[("conversation", 10_000), ("scratch", 10_000)]);
        let r = routing("conversation", &[], false, None); // persist = false
        apply_tool_results(
            &mut w,
            "resp",
            &[tc("c1", "read")],
            &[("c1".to_string(), "x".to_string())],
            Some(&r),
            None,
        );
        assert!(w.get_region("scratch").unwrap().current_tokens > 0);
    }

    #[test]
    fn apply_not_persist_without_scratch_uses_base_region() {
        let mut w = ctx(&[("conversation", 10_000)]); // no scratch region
        let r = routing("conversation", &[], false, None); // persist=false but no scratch
        apply_tool_results(
            &mut w,
            "r",
            &[tc("c1", "read")],
            &[("c1".to_string(), "x".to_string())],
            Some(&r),
            None,
        );
        assert!(w.get_region("conversation").unwrap().current_tokens > 0);
    }

    #[test]
    fn apply_truncates_per_max_result_tokens() {
        let mut w = ctx(&[("conversation", 10_000)]);
        let r = routing("conversation", &[], true, Some(1)); // 1 token ≈ 4 chars
        let long = "x".repeat(100);
        apply_tool_results(
            &mut w,
            "resp",
            &[tc("c1", "read")],
            &[("c1".to_string(), long)],
            Some(&r),
            None,
        );
        // Truncated, so the stored result is far smaller than 100 chars.
        assert!(w.get_region("conversation").unwrap().current_tokens < 25);
    }

    #[test]
    fn apply_no_truncation_when_result_under_max() {
        let mut w = ctx(&[("conversation", 10_000)]);
        let r = routing("conversation", &[], true, Some(100)); // budget 100 tok ≈ 400 chars
        apply_tool_results(
            &mut w,
            "r",
            &[tc("c1", "read")],
            &[("c1".to_string(), "short".to_string())], // 5 chars — under budget
            Some(&r),
            None,
        );
        assert!(w.get_region("conversation").unwrap().current_tokens > 0);
    }

    #[test]
    fn apply_tags_taint_when_sensitivities_present() {
        let mut w = ctx(&[("conversation", 10_000)]);
        let mut sens = std::collections::HashMap::new();
        sens.insert("read".to_string(), leviath_core::TaintLevel::Private);
        apply_tool_results(
            &mut w,
            "resp",
            &[tc("c1", "read")],
            &[("c1".to_string(), "x".to_string())],
            None,
            Some(&sens),
        );
        assert!(w.get_region("conversation").unwrap().current_tokens > 0);
    }

    #[test]
    fn apply_truncates_to_available_when_region_nearly_full() {
        let mut w = ctx(&[("conversation", 200)]);
        // Pre-fill so the tool result can't fit, but >100 tokens remain free.
        w.add_typed_entry(
            "conversation",
            leviath_core::EntryKind::UserMessage,
            "x".repeat(360),
            90,
        )
        .unwrap();
        let big = "y".repeat(600); // ~150 tokens — won't fit the ~110 remaining
        apply_tool_results(
            &mut w,
            "r",
            &[tc("c1", "read")],
            &[("c1".to_string(), big)],
            None,
            None,
        );
        // Result was truncated to fit (not dropped), staying within budget.
        let region = w.get_region("conversation").unwrap();
        assert!(region.current_tokens > 90 && region.current_tokens <= 200);
    }

    fn run_collect_tools(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(collect_tools);
        s.run(world);
    }

    #[test]
    fn collect_tools_applies_and_loops_back_to_infer() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(ToolResults(rx));
        let e = world
            .spawn((
                ctx(&[("conversation", 10_000)]),
                crate::components::InferenceResult {
                    response: "r".to_string(),
                    tool_calls: vec![tc("c1", "read")],
                    tokens_used: 0,
                    timestamp: 0,
                },
                AwaitingTools,
            ))
            .id();
        tx.send(ToolOutcome {
            entity: e,
            results: vec![("c1".to_string(), "res".to_string())],
        })
        .unwrap();

        run_collect_tools(&mut world);

        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<AwaitingTools>(e).is_none());
    }

    #[test]
    fn collect_tools_merges_stashed_context_results() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(ToolResults(rx));
        let e = world
            .spawn((
                ctx(&[("conversation", 10_000)]),
                infer_with(vec![ctx_call("c1", "notes", "hi"), tc("c2", "read")]),
                ContextToolResults(vec![("c1".to_string(), "stored".to_string())]),
                AwaitingTools,
            ))
            .id();
        tx.send(ToolOutcome {
            entity: e,
            results: vec![("c2".to_string(), "file body".to_string())],
        })
        .unwrap();

        run_collect_tools(&mut world);

        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<ContextToolResults>(e).is_none()); // consumed
        // Both results were written into context.
        assert!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .current_tokens
                > 0
        );
    }

    #[test]
    fn collect_tools_drops_stale_outcome() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(ToolResults(rx));
        let e = world.spawn(ctx(&[("conversation", 10_000)])).id(); // no AwaitingTools
        tx.send(ToolOutcome {
            entity: e,
            results: vec![],
        })
        .unwrap();

        run_collect_tools(&mut world);

        assert!(world.get::<ReadyToInfer>(e).is_none());
    }

    // ── message delivery ──

    fn msg(agent_id: &str, content: &str, region: Option<&str>) -> AgentMessage {
        AgentMessage {
            agent_id: agent_id.to_string(),
            content: content.to_string(),
            target_region: region.map(String::from),
            priority: 0,
        }
    }

    fn run_deliver(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(deliver_messages);
        s.run(world);
    }

    fn spawn_msg_agent(world: &mut World, accepts: bool, regions: &[(&str, usize)]) -> Entity {
        let mut state = agent_state();
        state.agent_id = "a1".to_string();
        state.accepts_messages = accepts;
        world
            .spawn((state, MessageInbox::default(), ctx(regions)))
            .id()
    }

    #[test]
    fn deliver_routes_and_delivers_to_accepting_agent() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(MessageIntake(rx));
        let e = spawn_msg_agent(&mut world, true, &[("conversation", 10_000)]);
        tx.send(msg("a1", "hello", None)).unwrap();

        run_deliver(&mut world);

        assert!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .current_tokens
                > 0
        );
        assert!(world.get::<MessageInbox>(e).unwrap().messages.is_empty());
    }

    #[test]
    fn deliver_holds_for_non_accepting_agent() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(MessageIntake(rx));
        let e = spawn_msg_agent(&mut world, false, &[("conversation", 10_000)]);
        tx.send(msg("a1", "hello", None)).unwrap();

        run_deliver(&mut world);

        // Not delivered — waits in the inbox for a stage that accepts messages.
        assert_eq!(world.get::<MessageInbox>(e).unwrap().messages.len(), 1);
        assert_eq!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .current_tokens,
            0
        );
    }

    #[test]
    fn deliver_drops_message_for_unknown_agent() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(MessageIntake(rx));
        let e = spawn_msg_agent(&mut world, true, &[("conversation", 10_000)]);
        tx.send(msg("nobody", "hi", None)).unwrap();

        run_deliver(&mut world);

        assert!(world.get::<MessageInbox>(e).unwrap().messages.is_empty());
        assert_eq!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .current_tokens,
            0
        );
    }

    #[test]
    fn deliver_honors_target_region() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(MessageIntake(rx));
        let e = spawn_msg_agent(
            &mut world,
            true,
            &[("conversation", 10_000), ("notes", 10_000)],
        );
        tx.send(msg("a1", "note this", Some("notes"))).unwrap();

        run_deliver(&mut world);

        assert!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("notes")
                .unwrap()
                .current_tokens
                > 0
        );
    }

    // ── transition resolution ──

    fn edge(
        target: &str,
        cond: leviath_core::blueprint::TransitionCondition,
    ) -> (String, leviath_core::blueprint::TransitionEdge) {
        (
            target.to_string(),
            leviath_core::blueprint::TransitionEdge {
                target: target.to_string(),
                condition: cond,
                hint: None,
                transform: leviath_core::blueprint::EdgeTransform::Direct,
            },
        )
    }

    fn stage_named(
        name: &str,
        edges: Option<Vec<(String, leviath_core::blueprint::TransitionEdge)>>,
        allow_complete: bool,
        max_revisits: Option<usize>,
    ) -> leviath_core::Stage {
        let mut s = leviath_core::Stage::new(
            name.to_string(),
            leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
        );
        s.allow_complete = allow_complete;
        s.max_revisits = max_revisits;
        if let Some(edges) = edges {
            s.transitions = Some(edges.into_iter().collect());
        }
        s
    }

    fn blueprint(stages: Vec<leviath_core::Stage>) -> leviath_core::Blueprint {
        let layout = leviath_core::layout::ContextLayout::new(
            vec![leviath_core::layout::RegionDefinition::new(
                "conversation".to_string(),
                RegionKind::Clearable,
                10_000,
            )],
            12_000,
        );
        leviath_core::Blueprint::new("t".to_string(), "d".to_string(), stages, layout)
    }

    fn si(model: &str) -> StageInference {
        StageInference {
            provider_name: "p".to_string(),
            model: model.to_string(),
            tools: vec![],
            tool_filter: None,
        }
    }

    /// A no-op stage setup (no layout, no system prompt, accepts input).
    fn setup() -> StageSetup {
        StageSetup {
            inference_config: InferenceConfig {
                temperature: None,
                max_output_tokens: None,
            },
            routing: None,
            accepts_messages: true,
            context_layout: None,
            system_prompt: None,
        }
    }

    fn setups(n: usize) -> StageSetups {
        StageSetups((0..n).map(|_| setup()).collect())
    }

    fn spawn_transition_agent(
        world: &mut World,
        bp: leviath_core::Blueprint,
        stage_infs: Vec<StageInference>,
        visits: VisitCounts,
    ) -> Entity {
        let n = stage_infs.len();
        world
            .spawn((
                AgentBlueprint(bp),
                StageCursor { index: 0 },
                agent_state(),
                StageProgress {
                    total_tool_calls: 3,
                    text_only_nudges: 1,
                    iterations: 0,
                },
                StageInferences(stage_infs),
                setups(n),
                conv_window(),
                visits,
                ResolveTransition,
            ))
            .id()
    }

    fn run_transition(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(resolve_transition);
        s.run(world);
    }

    #[test]
    fn transition_linear_advances_to_next_stage() {
        let bp = blueprint(vec![
            stage_named("a", None, false, None),
            stage_named("b", None, false, None),
        ]);
        let mut world = World::new();
        let e = spawn_transition_agent(
            &mut world,
            bp,
            vec![si("m0"), si("m1")],
            VisitCounts::default(),
        );

        run_transition(&mut world);

        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
        assert_eq!(world.get::<StageInference>(e).unwrap().model, "m1");
        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<ResolveTransition>(e).is_none());
        // Progress reset, visit bumped, current stage updated.
        assert_eq!(world.get::<StageProgress>(e).unwrap().total_tool_calls, 0);
        assert_eq!(world.get::<AgentState>(e).unwrap().current_stage, "b");
        assert_eq!(world.get::<VisitCounts>(e).unwrap().0.get("b"), Some(&1));
    }

    #[test]
    fn transition_terminal_marks_complete() {
        let bp = blueprint(vec![stage_named("only", None, false, None)]);
        let mut world = World::new();
        let e = spawn_transition_agent(&mut world, bp, vec![si("m")], VisitCounts::default());

        run_transition(&mut world);

        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Complete
        );
        assert!(world.get::<ResolveTransition>(e).is_none());
        assert!(world.get::<ReadyToInfer>(e).is_none());
    }

    #[test]
    fn transition_single_graph_edge_advances() {
        use leviath_core::blueprint::TransitionCondition;
        let bp = blueprint(vec![
            stage_named(
                "a",
                Some(vec![edge("b", TransitionCondition::Always)]),
                false,
                None,
            ),
            stage_named("b", None, false, None),
        ]);
        let mut world = World::new();
        let e = spawn_transition_agent(
            &mut world,
            bp,
            vec![si("m0"), si("m1")],
            VisitCounts::default(),
        );

        run_transition(&mut world);

        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
        assert!(world.get::<ReadyToInfer>(e).is_some());
    }

    #[test]
    fn transition_empty_transitions_is_terminal() {
        let bp = blueprint(vec![stage_named("a", Some(vec![]), false, None)]);
        let mut world = World::new();
        let e = spawn_transition_agent(&mut world, bp, vec![si("m")], VisitCounts::default());

        run_transition(&mut world);

        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Complete
        );
    }

    #[test]
    fn transition_multiple_edges_awaits_choice() {
        use leviath_core::blueprint::TransitionCondition;
        let bp = blueprint(vec![
            stage_named(
                "a",
                Some(vec![
                    edge("b", TransitionCondition::Always),
                    edge("c", TransitionCondition::Always),
                ]),
                false,
                None,
            ),
            stage_named("b", None, false, None),
            stage_named("c", None, false, None),
        ]);
        let mut world = World::new();
        let e = spawn_transition_agent(
            &mut world,
            bp,
            vec![si("m0"), si("m1"), si("m2")],
            VisitCounts::default(),
        );

        run_transition(&mut world);

        let choice = world.get::<AwaitingTransitionChoice>(e).unwrap();
        assert_eq!(choice.0.len(), 2);
        assert!(world.get::<ResolveTransition>(e).is_none());
        assert!(world.get::<ReadyToInfer>(e).is_none());
    }

    #[test]
    fn transition_allow_complete_single_edge_awaits_choice() {
        use leviath_core::blueprint::TransitionCondition;
        let bp = blueprint(vec![
            stage_named(
                "a",
                Some(vec![edge("b", TransitionCondition::Always)]),
                true, // allow_complete: LLM must be asked (can say DONE)
                None,
            ),
            stage_named("b", None, false, None),
        ]);
        let mut world = World::new();
        let e = spawn_transition_agent(
            &mut world,
            bp,
            vec![si("m0"), si("m1")],
            VisitCounts::default(),
        );

        run_transition(&mut world);

        assert!(world.get::<AwaitingTransitionChoice>(e).is_some());
    }

    #[test]
    fn transition_visit_exhausted_edge_is_terminal() {
        use leviath_core::blueprint::TransitionCondition;
        let bp = blueprint(vec![
            stage_named(
                "a",
                Some(vec![edge("b", TransitionCondition::Always)]),
                false,
                None,
            ),
            stage_named("b", None, false, Some(0)), // max_revisits 0
        ]);
        let mut visits = VisitCounts::default();
        visits.0.insert("b".to_string(), 1); // already visited past its budget
        let mut world = World::new();
        let e = spawn_transition_agent(&mut world, bp, vec![si("m0"), si("m1")], visits);

        run_transition(&mut world);

        // Only edge exhausted ⇒ terminal.
        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Complete
        );
    }

    #[test]
    fn transition_non_choosable_edge_is_terminal() {
        use leviath_core::blueprint::TransitionCondition;
        let bp = blueprint(vec![
            // Only an Error-condition edge, which isn't followable on a normal
            // completion ⇒ filtered out of the choosable set ⇒ terminal.
            stage_named(
                "a",
                Some(vec![edge("b", TransitionCondition::Error)]),
                false,
                None,
            ),
            stage_named("b", None, false, None),
        ]);
        let mut world = World::new();
        let e = spawn_transition_agent(
            &mut world,
            bp,
            vec![si("m0"), si("m1")],
            VisitCounts::default(),
        );

        run_transition(&mut world);

        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Complete
        );
    }

    #[test]
    fn transition_unknown_target_edge_is_terminal() {
        use leviath_core::blueprint::TransitionCondition;
        let bp = blueprint(vec![stage_named(
            "a",
            Some(vec![edge("ghost", TransitionCondition::Always)]),
            false,
            None,
        )]);
        let mut world = World::new();
        let e = spawn_transition_agent(&mut world, bp, vec![si("m0")], VisitCounts::default());

        run_transition(&mut world);

        // Edge points at a nonexistent stage ⇒ filtered ⇒ terminal.
        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Complete
        );
    }

    // ── stage setup on entry ──

    fn pinned_window() -> ContextWindow {
        let mut w = ContextWindow::new(10_000);
        w.add_region(Region::new("sys".to_string(), RegionKind::Pinned, 2000));
        w.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::Clearable,
            10_000,
        ));
        w
    }

    /// Spawn a linear two-stage agent poised to transition, with a custom setup
    /// for the destination stage and the given starting window.
    fn spawn_setup_agent(
        world: &mut World,
        dest_setup: StageSetup,
        window: ContextWindow,
    ) -> Entity {
        let bp = blueprint(vec![
            stage_named("a", None, false, None),
            stage_named("b", None, false, None),
        ]);
        world
            .spawn((
                AgentBlueprint(bp),
                StageCursor { index: 0 },
                agent_state(),
                StageProgress::default(),
                StageInferences(vec![si("m0"), si("m1")]),
                StageSetups(vec![setup(), dest_setup]),
                VisitCounts::default(),
                window,
                ResolveTransition,
            ))
            .id()
    }

    #[test]
    fn enter_stage_injects_system_prompt_and_config() {
        let mut s = setup();
        s.system_prompt = Some("be terse".to_string());
        s.inference_config = InferenceConfig {
            temperature: Some(0.3),
            max_output_tokens: Some(99),
        };
        s.accepts_messages = false;
        let mut world = World::new();
        let e = spawn_setup_agent(&mut world, s, pinned_window());

        run_transition(&mut world);

        // Instructions landed in the pinned region, not conversation.
        assert!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("sys")
                .unwrap()
                .current_tokens
                > 0
        );
        let cfg = world.get::<InferenceConfig>(e).unwrap();
        assert_eq!(cfg.max_output_tokens, Some(99));
        assert!(!world.get::<AgentState>(e).unwrap().accepts_messages);
        assert!(world.get::<ReadyToInfer>(e).is_some());
    }

    #[test]
    fn enter_stage_swaps_context_layout() {
        let mut s = setup();
        s.context_layout = Some(leviath_core::layout::ContextLayout::new(
            vec![leviath_core::layout::RegionDefinition::new(
                "scratch".to_string(),
                RegionKind::Clearable,
                5000,
            )],
            8000,
        ));
        let mut world = World::new();
        let e = spawn_setup_agent(&mut world, s, pinned_window());

        run_transition(&mut world);

        let w = world.get::<ContextWindow>(e).unwrap();
        assert!(w.get_region("scratch").is_some()); // swapped in
        assert!(w.get_region("sys").is_none()); // old layout dropped
    }

    #[test]
    fn enter_stage_inserts_tool_result_routing() {
        let mut s = setup();
        s.routing = Some(leviath_core::ToolResultRouting {
            default_region: "notes".to_string(),
            ..Default::default()
        });
        let mut world = World::new();
        let e = spawn_setup_agent(&mut world, s, pinned_window());

        run_transition(&mut world);

        let routing = world
            .get::<crate::components::ToolResultRoutingComponent>(e)
            .unwrap();
        assert_eq!(routing.routing.default_region, "notes");
    }

    #[test]
    fn enter_stage_errors_when_system_prompt_overflows_region() {
        let mut s = setup();
        s.system_prompt = Some("x".repeat(100_000)); // far exceeds the 2000-tok region
        let mut world = World::new();
        let e = spawn_setup_agent(&mut world, s, pinned_window());

        run_transition(&mut world);

        assert_eq!(
            std::mem::discriminant(&world.get::<AgentState>(e).unwrap().status),
            std::mem::discriminant(&AgentStatus::Error {
                message: String::new()
            })
        );
        assert!(world.get::<ReadyToInfer>(e).is_none());
    }

    #[test]
    fn enter_stage_without_target_region_skips_injection() {
        // Neither a pinned region nor a "conversation" region exists, so the
        // stage-instructions target ("conversation" fallback) isn't found: the
        // clear is skipped and, with no system prompt, entry still succeeds.
        let mut w = ContextWindow::new(10_000);
        w.add_region(Region::new(
            "notes".to_string(),
            RegionKind::Clearable,
            5000,
        ));
        let mut world = World::new();
        let e = spawn_setup_agent(&mut world, setup(), w);

        run_transition(&mut world);

        assert!(world.get::<ReadyToInfer>(e).is_some());
    }

    #[test]
    fn collect_choice_errors_when_system_prompt_overflows() {
        let (mut world, tx) = world_with_transition_results();
        let bp = blueprint(vec![
            stage_named("a", None, false, None),
            stage_named("b", None, false, None),
        ]);
        let mut dest = setup();
        dest.system_prompt = Some("x".repeat(100_000));
        let e = world
            .spawn((
                AgentBlueprint(bp),
                StageCursor { index: 0 },
                agent_state(),
                StageProgress::default(),
                StageInferences(vec![si("m0"), si("m1")]),
                StageSetups(vec![setup(), dest]),
                VisitCounts::default(),
                pinned_window(),
                AwaitingTransitionResponse(vec![plain_edge("b")]),
            ))
            .id();
        tx.send(InferenceOutcome {
            entity: e,
            result: Ok(resp("b")),
        })
        .unwrap();

        run_collect_transition(&mut world);

        assert_eq!(
            std::mem::discriminant(&world.get::<AgentState>(e).unwrap().status),
            std::mem::discriminant(&AgentStatus::Error {
                message: String::new()
            })
        );
        assert!(world.get::<AwaitingTransitionResponse>(e).is_none());
    }

    // ── agent spawn (blueprint → components) ──

    fn resolved(model: &str) -> ResolvedStage {
        ResolvedStage {
            provider_name: "p".to_string(),
            model: model.to_string(),
            tools: vec![],
        }
    }

    #[test]
    fn spawn_agent_builds_stage0_ready_with_config_and_routing() {
        // A stage with model parameters, routing, and a system prompt should
        // produce a ready agent carrying all of them.
        let layout = leviath_core::layout::ContextLayout::new(
            vec![leviath_core::layout::RegionDefinition::new(
                "task".to_string(),
                RegionKind::Pinned,
                4000,
            )],
            8000,
        );
        let mut s = leviath_core::Stage::new(
            "start".to_string(),
            leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
        );
        s.model
            .parameters
            .insert("temperature".to_string(), serde_json::json!(0.5));
        s.model
            .parameters
            .insert("max_output_tokens".to_string(), serde_json::json!(128));
        s.config.insert(
            "system_prompt".to_string(),
            serde_json::Value::String("be helpful".to_string()),
        );
        s.tool_result_routing = Some(leviath_core::ToolResultRouting {
            default_region: "notes".to_string(),
            ..Default::default()
        });
        let bp = leviath_core::Blueprint::new("t".to_string(), "d".to_string(), vec![s], layout);

        let mut world = World::new();
        let e = spawn_agent(
            &mut world,
            "agent-x".to_string(),
            bp,
            "the task",
            vec![resolved("m")],
        )
        .unwrap();

        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 0);
        let cfg = world.get::<InferenceConfig>(e).unwrap();
        assert_eq!(cfg.temperature, Some(0.5));
        assert_eq!(cfg.max_output_tokens, Some(128));
        assert_eq!(
            world
                .get::<crate::components::ToolResultRoutingComponent>(e)
                .unwrap()
                .routing
                .default_region,
            "notes"
        );
        assert_eq!(world.get::<AgentState>(e).unwrap().agent_id, "agent-x");
        // Stage 0's visit is pre-counted.
        assert_eq!(
            world.get::<VisitCounts>(e).unwrap().0.get("start"),
            Some(&1)
        );
        // Task text + system prompt both seeded the pinned region.
        assert!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("task")
                .unwrap()
                .current_tokens
                > 0
        );
    }

    #[test]
    fn spawn_agent_defaults_config_and_no_routing() {
        // No parameters, no routing, no system prompt → default config, no
        // routing component.
        let bp = blueprint(vec![stage_named("only", None, false, None)]);
        let mut world = World::new();
        let e = spawn_agent(&mut world, "a".to_string(), bp, "t", vec![resolved("m")]).unwrap();

        let cfg = world.get::<InferenceConfig>(e).unwrap();
        assert_eq!(cfg.temperature, None);
        assert_eq!(cfg.max_output_tokens, None);
        assert!(
            world
                .get::<crate::components::ToolResultRoutingComponent>(e)
                .is_none()
        );
    }

    #[test]
    fn stage_setup_from_folds_fanout_split_prompt() {
        use leviath_core::blueprint::{FanOutConfig, StageMode, WorkerFailurePolicy};
        let fanout = |split: &str| StageMode::FanOut {
            config: FanOutConfig {
                worker_agent: None,
                worker_stage: Some("w".to_string()),
                worker_query: None,
                merge_stage: None,
                max_workers: 4,
                on_worker_failure: WorkerFailurePolicy::Continue,
                split_prompt: split.to_string(),
            },
        };

        // Fan-out stage with a base prompt: split prompt is appended.
        let mut s = stage_named("fan", None, false, None);
        s.mode = fanout("SPLIT NOW");
        s.config.insert(
            "system_prompt".to_string(),
            serde_json::Value::String("base instructions".to_string()),
        );
        let sp = stage_setup_from(&s).system_prompt.unwrap();
        assert!(sp.contains("base instructions") && sp.contains("SPLIT NOW"));

        // Fan-out stage with no base prompt: the split prompt alone.
        let mut s2 = stage_named("fan", None, false, None);
        s2.mode = fanout("ONLY SPLIT");
        assert_eq!(
            stage_setup_from(&s2).system_prompt,
            Some("ONLY SPLIT".to_string())
        );

        // Fan-out stage with an empty split prompt: base prompt is left as-is.
        let mut s3 = stage_named("fan", None, false, None);
        s3.mode = fanout("   ");
        assert_eq!(stage_setup_from(&s3).system_prompt, None);
    }

    #[test]
    fn spawn_agent_errors_on_oversized_system_prompt() {
        let layout = leviath_core::layout::ContextLayout::new(
            vec![leviath_core::layout::RegionDefinition::new(
                "task".to_string(),
                RegionKind::Pinned,
                40,
            )],
            1000,
        );
        let mut s = leviath_core::Stage::new(
            "only".to_string(),
            leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
        );
        s.config.insert(
            "system_prompt".to_string(),
            serde_json::Value::String("z".repeat(100_000)),
        );
        let bp = leviath_core::Blueprint::new("t".to_string(), "d".to_string(), vec![s], layout);

        let mut world = World::new();
        let err = spawn_agent(&mut world, "a".to_string(), bp, "t", vec![resolved("m")]);
        assert!(err.is_err());
    }

    // ── compaction ──

    fn compacting_window() -> ContextWindow {
        let mut w = ContextWindow::new(100);
        let mut conv = Region::new(
            "conv".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 5,
            },
            100,
        );
        let _ = conv.add_entry("x".repeat(380), 95); // 95 tokens: over threshold, <10 free
        w.add_region(conv);
        w.add_region(Region::new(
            "history".to_string(),
            RegionKind::CompactHistory {
                source_region: "conv".to_string(),
            },
            100,
        ));
        w.current_tokens = w.calculate_tokens();
        w
    }

    fn compaction_settings(provider: &str, model: &str) -> CompactionSettings {
        CompactionSettings(leviath_core::CompactionConfig {
            provider: provider.to_string(),
            model: model.to_string(),
            system_prompt: None,
            user_prompt_template: None,
            max_summary_tokens: 200,
            temperature: 0.2,
        })
    }

    fn run_dispatch_compaction(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(dispatch_compaction);
        s.run(world);
    }

    #[tokio::test]
    async fn compaction_dispatches_when_over_threshold() {
        // Provider "cfg" is registered by build_world; the window is at the
        // eviction threshold with a Compacting region that needs summarizing.
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let e = world
            .spawn((
                compacting_window(),
                compaction_settings("cfg", "m"),
                agent_state(),
                ReadyToInfer,
            ))
            .id();

        run_dispatch_compaction(&mut world);

        assert!(world.get::<AwaitingCompaction>(e).is_some());
        assert!(world.get::<ReadyToInfer>(e).is_none());
    }

    #[tokio::test]
    async fn compaction_skips_non_active_agent() {
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let mut st = agent_state();
        st.status = AgentStatus::Idle;
        let e = world
            .spawn((
                compacting_window(),
                compaction_settings("cfg", "m"),
                st,
                ReadyToInfer,
            ))
            .id();

        run_dispatch_compaction(&mut world);

        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<AwaitingCompaction>(e).is_none());
    }

    #[tokio::test]
    async fn compaction_skips_when_under_threshold() {
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let mut w = ContextWindow::new(1000);
        w.add_region(Region::new(
            "conv".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 5,
            },
            1000,
        ));
        let e = world
            .spawn((
                w,
                compaction_settings("cfg", "m"),
                agent_state(),
                ReadyToInfer,
            ))
            .id();

        run_dispatch_compaction(&mut world);

        // Under threshold ⇒ untouched, ready to infer.
        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<AwaitingCompaction>(e).is_none());
    }

    #[tokio::test]
    async fn compaction_skips_when_provider_missing() {
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let e = world
            .spawn((
                compacting_window(),
                compaction_settings("ghost", "m"), // unregistered provider
                agent_state(),
                ReadyToInfer,
            ))
            .id();

        run_dispatch_compaction(&mut world);

        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<AwaitingCompaction>(e).is_none());
    }

    #[tokio::test]
    async fn compaction_skips_when_pool_full() {
        let mut cfg = InferencePoolConfig::new();
        cfg.set_limit("m", 0); // no permits for the compaction model
        let (mut world, _rx) = build_world(InferencePools::new(cfg));
        let e = world
            .spawn((
                compacting_window(),
                compaction_settings("cfg", "m"),
                agent_state(),
                ReadyToInfer,
            ))
            .id();

        run_dispatch_compaction(&mut world);

        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<AwaitingCompaction>(e).is_none());
    }

    #[tokio::test]
    async fn compaction_evicts_but_needs_no_summary() {
        // A Clearable region over threshold is fully cleared by sync eviction, so
        // no LLM summary is needed and the agent stays ready to infer.
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let mut w = ContextWindow::new(100);
        let mut scratch = Region::new("scratch".to_string(), RegionKind::Clearable, 100);
        let _ = scratch.add_entry("y".repeat(360), 95);
        w.add_region(scratch);
        w.current_tokens = w.calculate_tokens();
        let e = world
            .spawn((
                w,
                compaction_settings("cfg", "m"),
                agent_state(),
                ReadyToInfer,
            ))
            .id();

        run_dispatch_compaction(&mut world);

        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<AwaitingCompaction>(e).is_none());
        // The clearable region was emptied by eviction.
        assert_eq!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("scratch")
                .unwrap()
                .current_tokens,
            0
        );
    }

    #[tokio::test]
    async fn compaction_skips_when_eviction_errors() {
        // Pinned content over the total budget makes try_evict return
        // PinnedRegionsOverBudget; compaction is skipped and inference proceeds.
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let mut w = ContextWindow::new(100);
        let mut pinned = Region::new("id".to_string(), RegionKind::Pinned, 500);
        let _ = pinned.add_entry("p".repeat(600), 150); // pinned 150 > budget 100
        w.add_region(pinned);
        w.current_tokens = w.calculate_tokens();
        let e = world
            .spawn((
                w,
                compaction_settings("cfg", "m"),
                agent_state(),
                ReadyToInfer,
            ))
            .id();

        run_dispatch_compaction(&mut world);

        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<AwaitingCompaction>(e).is_none());
    }

    #[tokio::test]
    async fn compaction_skips_region_with_empty_content() {
        // A Compacting region over its token threshold but whose entries carry no
        // text (a token-only placeholder) yields nothing to summarize.
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let mut w = ContextWindow::new(100);
        let mut conv = Region::new(
            "conv".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 5,
            },
            100,
        );
        let _ = conv.add_entry(String::new(), 95); // empty content, 95 tokens
        w.add_region(conv);
        w.current_tokens = w.calculate_tokens();
        let e = world
            .spawn((
                w,
                compaction_settings("cfg", "m"),
                agent_state(),
                ReadyToInfer,
            ))
            .id();

        run_dispatch_compaction(&mut world);

        // Nothing summarizable ⇒ no job, stays ready.
        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<AwaitingCompaction>(e).is_none());
    }

    // ── edge transforms ──

    use leviath_core::blueprint::EdgeTransform;

    /// A window with a pinned `sys` region and a stage-specific `scratch` region,
    /// both with content.
    fn transform_window() -> ContextWindow {
        let mut w = ContextWindow::new(1000);
        let mut sys = Region::new("sys".to_string(), RegionKind::Pinned, 500);
        let _ = sys.add_entry("identity".to_string(), 10);
        w.add_region(sys);
        let mut scratch = Region::new("scratch".to_string(), RegionKind::Clearable, 500);
        let _ = scratch.add_entry("work".to_string(), 10);
        w.add_region(scratch);
        w.current_tokens = w.calculate_tokens();
        w
    }

    #[test]
    fn apply_edge_transform_direct_is_a_noop() {
        let mut w = transform_window();
        let before = w.current_tokens;
        assert!(apply_edge_transform(&mut w, &EdgeTransform::Direct).is_empty());
        assert_eq!(w.current_tokens, before);
        assert!(w.get_region("scratch").unwrap().current_tokens > 0);
    }

    #[test]
    fn apply_edge_transform_clear_wipes_stage_specific_keeps_pinned() {
        let mut w = transform_window();
        assert!(apply_edge_transform(&mut w, &EdgeTransform::Clear).is_empty());
        assert_eq!(w.get_region("scratch").unwrap().current_tokens, 0);
        assert!(w.get_region("sys").unwrap().current_tokens > 0);
    }

    #[test]
    fn apply_edge_transform_compact_returns_stage_specific_with_content() {
        let mut w = transform_window();
        // Pinned excluded; scratch (stage-specific, has content) returned; not cleared.
        assert_eq!(
            apply_edge_transform(&mut w, &EdgeTransform::Compact { prompt: None }),
            vec!["scratch".to_string()]
        );
        assert!(w.get_region("scratch").unwrap().current_tokens > 0);
    }

    #[test]
    fn apply_edge_transform_custom_respects_carry_clear_and_compact() {
        let mut w = transform_window();
        let mut keep = Region::new("keep".to_string(), RegionKind::Clearable, 500);
        let _ = keep.add_entry("keepme".to_string(), 10);
        w.add_region(keep);
        let mut drop = Region::new("drop".to_string(), RegionKind::Clearable, 500);
        let _ = drop.add_entry("dropme".to_string(), 10);
        w.add_region(drop);
        w.current_tokens = w.calculate_tokens();

        let transform = EdgeTransform::Custom {
            carry: vec!["keep".to_string()],
            // scratch has content ⇒ kept; keep excluded (carry); ghost absent ⇒ filtered.
            compact: vec![
                "scratch".to_string(),
                "keep".to_string(),
                "ghost".to_string(),
            ],
            // drop cleared; keep protected by carry; missing region is a no-op.
            clear: vec![
                "drop".to_string(),
                "keep".to_string(),
                "missing".to_string(),
            ],
            compact_prompt: None,
        };
        let out = apply_edge_transform(&mut w, &transform);
        assert_eq!(w.get_region("drop").unwrap().current_tokens, 0);
        assert!(w.get_region("keep").unwrap().current_tokens > 0);
        assert_eq!(out, vec!["scratch".to_string()]);
    }

    /// A window with a stage-specific `scratch` region carrying summarizable text.
    fn scratch_window() -> ContextWindow {
        let mut w = ContextWindow::new(1000);
        let mut scratch = Region::new("scratch".to_string(), RegionKind::Clearable, 500);
        let _ = scratch.add_entry("work to summarize".to_string(), 20);
        w.add_region(scratch);
        w.current_tokens = w.calculate_tokens();
        w
    }

    fn run_dispatch_edge_compact(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(dispatch_edge_compact);
        s.run(world);
    }

    #[tokio::test]
    async fn edge_compact_dispatches_to_the_compaction_lane() {
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let e = world
            .spawn((
                scratch_window(),
                PendingEdgeCompact(vec!["scratch".to_string()]),
                compaction_settings("cfg", "m"),
                agent_state(),
                ReadyToInfer,
            ))
            .id();
        run_dispatch_edge_compact(&mut world);
        assert!(world.get::<AwaitingCompaction>(e).is_some());
        assert!(world.get::<ReadyToInfer>(e).is_none());
        assert!(world.get::<PendingEdgeCompact>(e).is_none());
    }

    #[tokio::test]
    async fn edge_compact_skips_non_active_agent() {
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let mut st = agent_state();
        st.status = AgentStatus::Cancelled;
        let e = world
            .spawn((
                scratch_window(),
                PendingEdgeCompact(vec!["scratch".to_string()]),
                compaction_settings("cfg", "m"),
                st,
                ReadyToInfer,
            ))
            .id();
        run_dispatch_edge_compact(&mut world);
        // Left untouched (marker preserved) for when it resumes.
        assert!(world.get::<PendingEdgeCompact>(e).is_some());
        assert!(world.get::<AwaitingCompaction>(e).is_none());
    }

    #[tokio::test]
    async fn edge_compact_drops_marker_without_compaction_settings() {
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let e = world
            .spawn((
                scratch_window(),
                PendingEdgeCompact(vec!["scratch".to_string()]),
                agent_state(),
                ReadyToInfer,
            ))
            .id();
        run_dispatch_edge_compact(&mut world);
        // No settings ⇒ can't summarize ⇒ drop the request, proceed to inference.
        assert!(world.get::<PendingEdgeCompact>(e).is_none());
        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<AwaitingCompaction>(e).is_none());
    }

    #[tokio::test]
    async fn edge_compact_drops_marker_when_nothing_to_summarize() {
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        // A present-but-empty region + an absent region ⇒ no requests.
        let mut w = ContextWindow::new(1000);
        let mut empty = Region::new("empty".to_string(), RegionKind::Clearable, 500);
        let _ = empty.add_entry(String::new(), 5);
        w.add_region(empty);
        let e = world
            .spawn((
                w,
                PendingEdgeCompact(vec!["empty".to_string(), "ghost".to_string()]),
                compaction_settings("cfg", "m"),
                agent_state(),
                ReadyToInfer,
            ))
            .id();
        run_dispatch_edge_compact(&mut world);
        assert!(world.get::<PendingEdgeCompact>(e).is_none());
        assert!(world.get::<AwaitingCompaction>(e).is_none());
    }

    #[tokio::test]
    async fn edge_compact_drops_marker_when_provider_missing() {
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let e = world
            .spawn((
                scratch_window(),
                PendingEdgeCompact(vec!["scratch".to_string()]),
                compaction_settings("ghost", "m"), // unregistered provider
                agent_state(),
                ReadyToInfer,
            ))
            .id();
        run_dispatch_edge_compact(&mut world);
        assert!(world.get::<PendingEdgeCompact>(e).is_none());
        assert!(world.get::<AwaitingCompaction>(e).is_none());
    }

    #[tokio::test]
    async fn edge_compact_drops_marker_when_pool_full() {
        let mut cfg = InferencePoolConfig::new();
        cfg.set_limit("m", 0);
        let (mut world, _rx) = build_world(InferencePools::new(cfg));
        let e = world
            .spawn((
                scratch_window(),
                PendingEdgeCompact(vec!["scratch".to_string()]),
                compaction_settings("cfg", "m"),
                agent_state(),
                ReadyToInfer,
            ))
            .id();
        run_dispatch_edge_compact(&mut world);
        assert!(world.get::<PendingEdgeCompact>(e).is_none());
        assert!(world.get::<AwaitingCompaction>(e).is_none());
    }

    fn clear_edge(target: &str) -> leviath_core::blueprint::TransitionEdge {
        leviath_core::blueprint::TransitionEdge {
            target: target.to_string(),
            condition: leviath_core::blueprint::TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Clear,
        }
    }

    #[test]
    fn resolve_transition_applies_the_edge_clear_transform() {
        let a = stage_named(
            "a",
            Some(vec![("go".to_string(), clear_edge("b"))]),
            false,
            None,
        );
        let b = stage_named("b", None, false, None);
        let bp = blueprint(vec![a, b]);
        let mut world = World::new();
        let e = spawn_transition_agent(
            &mut world,
            bp,
            vec![stage("m", vec![], None), stage("m", vec![], None)],
            VisitCounts::default(),
        );
        // Seed content so the Clear transform has something to wipe.
        world
            .get_mut::<ContextWindow>(e)
            .unwrap()
            .add_to_region("conversation", "chatter".to_string(), 10)
            .unwrap();
        run_transition(&mut world);

        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1); // entered b
        assert_eq!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .current_tokens,
            0 // Clear transform wiped it
        );
        assert!(world.get::<PendingEdgeCompact>(e).is_none()); // Clear needs no LLM
    }

    #[test]
    fn resolve_transition_with_compact_transform_marks_pending_edge_compact() {
        let mut edge = clear_edge("b");
        edge.transform = EdgeTransform::Compact { prompt: None };
        let a = stage_named("a", Some(vec![("go".to_string(), edge)]), false, None);
        let b = stage_named("b", None, false, None);
        let bp = blueprint(vec![a, b]);
        let mut world = World::new();
        let e = spawn_transition_agent(
            &mut world,
            bp,
            vec![stage("m", vec![], None), stage("m", vec![], None)],
            VisitCounts::default(),
        );
        world
            .get_mut::<ContextWindow>(e)
            .unwrap()
            .add_to_region("conversation", "summarize me".to_string(), 10)
            .unwrap();
        run_transition(&mut world);

        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
        // The Compact transform queued the conversation region for the LLM lane.
        let pending = world.get::<PendingEdgeCompact>(e).unwrap();
        assert_eq!(pending.0, vec!["conversation".to_string()]);
    }

    // ── max_iterations + error/max-iter edges (#3+#4) ──

    use leviath_core::blueprint::TransitionCondition;

    fn conditioned_edge(
        target: &str,
        condition: TransitionCondition,
    ) -> leviath_core::blueprint::TransitionEdge {
        let mut e = plain_edge(target);
        e.condition = condition;
        e
    }

    fn spawn_ready_agent(
        world: &mut World,
        max_iterations: Option<usize>,
        iterations: usize,
        status: AgentStatus,
    ) -> Entity {
        let mut s = stage_named("a", None, false, None);
        s.max_iterations = max_iterations;
        let bp = blueprint(vec![s]);
        world
            .spawn((
                AgentBlueprint(bp),
                StageCursor { index: 0 },
                AgentState {
                    status,
                    ..agent_state()
                },
                StageProgress {
                    iterations,
                    ..Default::default()
                },
                ReadyToInfer,
            ))
            .id()
    }

    fn run_enforce(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(enforce_max_iterations);
        s.run(world);
    }

    #[test]
    fn enforce_max_iterations_caps_at_the_limit() {
        let mut world = World::new();
        let e = spawn_ready_agent(&mut world, Some(3), 3, AgentStatus::Active);
        run_enforce(&mut world);
        assert!(world.get::<ResolveTransition>(e).is_some());
        assert!(world.get::<ReadyToInfer>(e).is_none());
        assert_eq!(
            world.get::<StageOutcome>(e).unwrap(),
            &StageOutcome::MaxIterations
        );
    }

    #[test]
    fn enforce_max_iterations_below_limit_or_unlimited_or_paused_is_noop() {
        let mut world = World::new();
        let below = spawn_ready_agent(&mut world, Some(5), 2, AgentStatus::Active);
        let unlimited = spawn_ready_agent(&mut world, None, 99, AgentStatus::Active);
        let zero = spawn_ready_agent(&mut world, Some(0), 99, AgentStatus::Active);
        let paused = spawn_ready_agent(&mut world, Some(1), 99, AgentStatus::Idle);
        run_enforce(&mut world);
        for e in [below, unlimited, zero, paused] {
            assert!(world.get::<ReadyToInfer>(e).is_some());
            assert!(world.get::<ResolveTransition>(e).is_none());
        }
    }

    #[test]
    fn find_conditioned_edge_matches_condition_target_and_budget() {
        let err = conditioned_edge("recovery", TransitionCondition::Error);
        let a = stage_named("a", Some(vec![("e".to_string(), err)]), false, None);
        let recovery = stage_named("recovery", None, false, None);
        let bp = blueprint(vec![a, recovery]);
        let visits = std::collections::HashMap::new();
        assert_eq!(
            find_conditioned_edge(&bp, &bp.stages[0], &visits, TransitionCondition::Error)
                .map(|(i, _)| i),
            Some(1)
        );
        // No max_iterations edge present.
        assert!(
            find_conditioned_edge(
                &bp,
                &bp.stages[0],
                &visits,
                TransitionCondition::MaxIterations
            )
            .is_none()
        );
        // A stage with no transitions at all yields nothing.
        let none_bp = blueprint(vec![stage_named("solo", None, false, None)]);
        assert!(
            find_conditioned_edge(
                &none_bp,
                &none_bp.stages[0],
                &visits,
                TransitionCondition::Error
            )
            .is_none()
        );
    }

    #[test]
    fn find_conditioned_edge_skips_unknown_target_and_exhausted_revisits() {
        let ghost = conditioned_edge("nope", TransitionCondition::Error);
        let a = stage_named("a", Some(vec![("g".to_string(), ghost)]), false, None);
        let bp = blueprint(vec![a]);
        let visits = std::collections::HashMap::new();
        assert!(
            find_conditioned_edge(&bp, &bp.stages[0], &visits, TransitionCondition::Error)
                .is_none()
        );

        // Target exists but its revisit budget is exhausted.
        let err = conditioned_edge("recovery", TransitionCondition::Error);
        let a2 = stage_named("a", Some(vec![("e".to_string(), err)]), false, None);
        let recovery = stage_named("recovery", None, false, Some(0));
        let bp2 = blueprint(vec![a2, recovery]);
        let mut visited = std::collections::HashMap::new();
        visited.insert("recovery".to_string(), 1);
        assert!(
            find_conditioned_edge(&bp2, &bp2.stages[0], &visited, TransitionCondition::Error)
                .is_none()
        );
    }

    fn spawn_outcome_agent(
        world: &mut World,
        bp: leviath_core::Blueprint,
        outcome: StageOutcome,
        status: AgentStatus,
    ) -> Entity {
        let n = bp.stages.len();
        let infs: Vec<StageInference> = (0..n).map(|_| stage("m", vec![], None)).collect();
        let e = spawn_transition_agent(world, bp, infs, VisitCounts::default());
        world
            .entity_mut(e)
            .insert(outcome)
            .get_mut::<AgentState>()
            .unwrap()
            .status = status;
        e
    }

    #[test]
    fn resolve_transition_routes_error_to_error_edge() {
        let err = conditioned_edge("recovery", TransitionCondition::Error);
        let a = stage_named("a", Some(vec![("e".to_string(), err)]), false, None);
        let recovery = stage_named("recovery", None, false, None);
        let bp = blueprint(vec![a, recovery]);
        let mut world = World::new();
        let e = spawn_outcome_agent(
            &mut world,
            bp,
            StageOutcome::Errored("boom".to_string()),
            AgentStatus::Error {
                message: "boom".to_string(),
            },
        );
        run_transition(&mut world);
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1); // entered recovery
        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Active
        );
        assert!(world.get::<StageOutcome>(e).is_none());
    }

    #[test]
    fn resolve_transition_errors_terminally_without_an_error_edge() {
        // Stage 'a' has only an Always edge to 'b' — no error edge.
        let a = stage_named(
            "a",
            Some(vec![("go".to_string(), plain_edge("b"))]),
            false,
            None,
        );
        let b = stage_named("b", None, false, None);
        let bp = blueprint(vec![a, b]);
        let mut world = World::new();
        let e = spawn_outcome_agent(
            &mut world,
            bp,
            StageOutcome::Errored("boom".to_string()),
            AgentStatus::Error {
                message: "boom".to_string(),
            },
        );
        run_transition(&mut world);
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 0); // no transition
        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Error {
                message: "boom".to_string()
            }
        );
        assert!(world.get::<StageOutcome>(e).is_none());
        assert!(world.get::<ResolveTransition>(e).is_none());
    }

    #[test]
    fn resolve_transition_routes_max_iterations_edge_else_falls_through() {
        // With a max_iterations edge → follow it.
        let mi = conditioned_edge("recovery", TransitionCondition::MaxIterations);
        let a = stage_named("a", Some(vec![("m".to_string(), mi)]), false, None);
        let recovery = stage_named("recovery", None, false, None);
        let bp = blueprint(vec![a, recovery]);
        let mut world = World::new();
        let e = spawn_outcome_agent(
            &mut world,
            bp,
            StageOutcome::MaxIterations,
            AgentStatus::Active,
        );
        run_transition(&mut world);
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);

        // Without one → fall through to a normal (linear) transition.
        let a2 = stage_named("a", None, false, None);
        let b2 = stage_named("b", None, false, None);
        let bp2 = blueprint(vec![a2, b2]);
        let mut world2 = World::new();
        let e2 = spawn_outcome_agent(
            &mut world2,
            bp2,
            StageOutcome::MaxIterations,
            AgentStatus::Active,
        );
        run_transition(&mut world2);
        assert_eq!(world2.get::<StageCursor>(e2).unwrap().index, 1); // linear fall-through
        assert!(world2.get::<StageOutcome>(e2).is_none());
    }

    // ── required-region gating (#5) ──

    fn required_bp(tools: &[&str], custom_msg: Option<&str>) -> AgentBlueprint {
        let region = leviath_core::layout::RegionDefinition::new(
            "plan".to_string(),
            RegionKind::Pinned,
            4000,
        )
        .with_required(true, custom_msg.map(str::to_string));
        let layout = leviath_core::layout::ContextLayout::new(vec![region], 10_000);
        let mut stage = stage_named("a", None, false, None);
        stage.available_tools = tools.iter().map(|s| s.to_string()).collect();
        stage.context_layout = Some(layout.clone());
        AgentBlueprint(leviath_core::Blueprint::new(
            "t".to_string(),
            "d".to_string(),
            vec![stage],
            layout,
        ))
    }

    fn window_with_plan(filled: bool) -> ContextWindow {
        let mut w = ContextWindow::new(100_000);
        w.add_region(Region::new("plan".to_string(), RegionKind::Pinned, 4000));
        w.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::Clearable,
            10_000,
        ));
        if filled {
            w.add_to_region("plan", "the plan".to_string(), 5).unwrap();
        }
        w
    }

    #[test]
    fn unmet_required_regions_flags_empty_clears_when_filled_and_skips_without_tool() {
        let bp = required_bp(&["context_write"], None);
        assert_eq!(
            unmet_required_regions(&bp.0, &bp.0.stages[0], &window_with_plan(false)).len(),
            1
        );
        assert!(unmet_required_regions(&bp.0, &bp.0.stages[0], &window_with_plan(true)).is_empty());
        // No context-writing tool ⇒ never gated (would loop pointlessly).
        let no_tool = required_bp(&["read_file"], None);
        assert!(
            unmet_required_regions(&no_tool.0, &no_tool.0.stages[0], &window_with_plan(false))
                .is_empty()
        );
        // A required region absent from the window entirely counts as unmet.
        let mut bare = ContextWindow::new(100_000);
        bare.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::Clearable,
            10_000,
        ));
        assert_eq!(
            unmet_required_regions(&bp.0, &bp.0.stages[0], &bare).len(),
            1
        );
    }

    #[test]
    fn unmet_required_regions_falls_back_to_blueprint_layout() {
        // The stage has no per-stage layout, so the blueprint's layout is used.
        let mut bp = required_bp(&["context_write"], None);
        bp.0.stages[0].context_layout = None;
        assert_eq!(
            unmet_required_regions(&bp.0, &bp.0.stages[0], &window_with_plan(false)).len(),
            1
        );
    }

    fn run_require(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(require_context_regions);
        s.run(world);
    }

    #[test]
    fn require_context_regions_reruns_stage_on_unmet() {
        let mut world = World::new();
        let e = world
            .spawn((
                required_bp(&["context_write"], Some("write the plan!")),
                StageCursor { index: 0 },
                window_with_plan(false),
                ResolveTransition,
            ))
            .id();
        run_require(&mut world);
        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<ResolveTransition>(e).is_none());
        assert_eq!(world.get::<RequiredReentries>(e).unwrap().0, 1);
        // The custom nudge was injected into conversation.
        assert!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .current_tokens
                > 0
        );
    }

    #[test]
    fn require_context_regions_injects_default_message() {
        // No custom required_message ⇒ the default nudge text is used.
        let mut world = World::new();
        let e = world
            .spawn((
                required_bp(&["context_write"], None),
                StageCursor { index: 0 },
                window_with_plan(false),
                ResolveTransition,
            ))
            .id();
        run_require(&mut world);
        let conv = world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .content
            .iter()
            .map(|entry| entry.content.clone())
            .collect::<String>();
        assert!(conv.contains("Required context region 'plan' is still empty"));
    }

    #[test]
    fn require_context_regions_proceeds_when_met_capped_or_errored() {
        let mut world = World::new();
        // met ⇒ proceed
        let met = world
            .spawn((
                required_bp(&["context_write"], None),
                StageCursor { index: 0 },
                window_with_plan(true),
                ResolveTransition,
            ))
            .id();
        // unmet but at the cap ⇒ proceed with a warning
        let capped = world
            .spawn((
                required_bp(&["context_write"], None),
                StageCursor { index: 0 },
                window_with_plan(false),
                RequiredReentries(DEFAULT_REQUIRED_REENTRY_CAP),
                ResolveTransition,
            ))
            .id();
        // unmet but the stage errored ⇒ the error transition takes precedence
        let errored = world
            .spawn((
                required_bp(&["context_write"], None),
                StageCursor { index: 0 },
                window_with_plan(false),
                StageOutcome::Errored("boom".to_string()),
                ResolveTransition,
            ))
            .id();
        run_require(&mut world);
        for e in [met, capped, errored] {
            assert!(world.get::<ResolveTransition>(e).is_some());
            assert!(world.get::<ReadyToInfer>(e).is_none());
        }
    }

    // ── file tracking (#6) ──

    fn ftc(
        reads: bool,
        writes: bool,
        max: Option<usize>,
    ) -> leviath_core::blueprint::FileTrackingConfig {
        leviath_core::blueprint::FileTrackingConfig {
            region: "files".to_string(),
            track_reads: reads,
            track_writes: writes,
            max_file_tokens: max,
        }
    }

    fn fcall(id: &str, name: &str, args: serde_json::Value) -> crate::components::ToolCall {
        crate::components::ToolCall {
            tool_id: id.to_string(),
            name: name.to_string(),
            arguments: args,
        }
    }

    fn hashmap_window() -> ContextWindow {
        let mut w = ContextWindow::new(100_000);
        w.add_region(Region::new(
            "files".to_string(),
            RegionKind::HashMap { max_entries: None },
            40_000,
        ));
        w
    }

    #[test]
    fn truncate_file_caps_only_when_over_the_limit() {
        assert_eq!(truncate_file("short".to_string(), Some(100)), "short");
        assert_eq!(truncate_file("short".to_string(), None), "short");
        let out = truncate_file("x".repeat(500), Some(10)); // 10*4 = 40 chars
        assert!(out.contains("truncated at 10 tokens"));
        assert!(out.len() < 500);
    }

    #[test]
    fn apply_file_tracking_tracks_reads_and_writes() {
        let ft = ftc(true, true, Some(2)); // small cap to also exercise truncation
        let mut w = hashmap_window();
        let calls = vec![
            fcall("1", "read_file", serde_json::json!({"path": "a.rs"})),
            fcall(
                "2",
                "write_file",
                serde_json::json!({"path": "b.rs", "content": "fn b() {}"}),
            ),
        ];
        let mut merged = vec![
            ("1".to_string(), "fn a() { /* long body */ }".to_string()),
            ("2".to_string(), "written ok".to_string()),
        ];
        apply_file_tracking(&mut w, &ft, &calls, &mut merged);
        assert!(merged[0].1.contains("Reference it there"));
        assert!(merged[1].1.contains("Reference it there"));
        assert_eq!(w.get_region("files").unwrap().content.len(), 2);
    }

    #[test]
    fn apply_file_tracking_noop_without_a_hashmap_region() {
        let ft = ftc(true, true, None);
        let calls = vec![fcall("1", "read_file", serde_json::json!({"path": "a"}))];
        let mut merged = vec![("1".to_string(), "body".to_string())];
        // No "files" region at all.
        let mut w1 = ContextWindow::new(100_000);
        apply_file_tracking(&mut w1, &ft, &calls, &mut merged);
        assert_eq!(merged[0].1, "body");
        // "files" region exists but isn't a HashMap.
        let mut w2 = ContextWindow::new(100_000);
        w2.add_region(Region::new(
            "files".to_string(),
            RegionKind::Clearable,
            40_000,
        ));
        apply_file_tracking(&mut w2, &ft, &calls, &mut merged);
        assert_eq!(merged[0].1, "body");
    }

    #[test]
    fn apply_file_tracking_skips_errors_missing_path_other_tools_and_flags() {
        let mut w = hashmap_window();
        let ft = ftc(true, true, None);
        let calls = vec![
            fcall("1", "read_file", serde_json::json!({"path": "a"})), // result is an error
            fcall("2", "read_file", serde_json::json!({})),            // no path
            fcall("3", "list_dir", serde_json::json!({"path": "d"})),  // untracked tool
            fcall("4", "write_file", serde_json::json!({"path": "e"})), // no content
            fcall("5", "read_file", serde_json::json!({"path": "f"})), // result is denied
        ];
        let mut merged = vec![
            ("1".to_string(), "[error] boom".to_string()),
            ("2".to_string(), "body".to_string()),
            ("3".to_string(), "listing".to_string()),
            ("4".to_string(), "written".to_string()),
            ("5".to_string(), "[denied] nope".to_string()),
        ];
        apply_file_tracking(&mut w, &ft, &calls, &mut merged);
        for (_, r) in &merged {
            assert!(!r.contains("Reference it there"));
        }
        assert_eq!(w.get_region("files").unwrap().content.len(), 0);

        // With tracking flags off, read/write are also skipped.
        let off = ftc(false, false, None);
        let calls2 = vec![
            fcall("1", "read_file", serde_json::json!({"path": "a"})),
            fcall(
                "2",
                "write_file",
                serde_json::json!({"path": "b", "content": "x"}),
            ),
        ];
        let mut merged2 = vec![
            ("1".to_string(), "body".to_string()),
            ("2".to_string(), "written".to_string()),
        ];
        apply_file_tracking(&mut w, &off, &calls2, &mut merged2);
        for (_, r) in &merged2 {
            assert!(!r.contains("Reference it there"));
        }
    }

    #[test]
    fn collect_tools_applies_file_tracking_from_blueprint() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(ToolResults(rx));
        let mut w = hashmap_window();
        w.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::Clearable,
            10_000,
        ));
        // A blueprint carrying a file_tracking config.
        let layout = leviath_core::layout::ContextLayout::new(vec![], 10_000);
        let mut bp = leviath_core::Blueprint::new(
            "t".to_string(),
            "d".to_string(),
            vec![stage_named("a", None, false, None)],
            layout,
        );
        bp.file_tracking = Some(ftc(true, true, None));
        let e = world
            .spawn((
                w,
                infer_with(vec![fcall(
                    "c1",
                    "read_file",
                    serde_json::json!({"path": "a.rs"}),
                )]),
                AwaitingTools,
                AgentBlueprint(bp),
            ))
            .id();
        tx.send(ToolOutcome {
            entity: e,
            results: vec![("c1".to_string(), "fn a() {}".to_string())],
        })
        .unwrap();
        run_collect_tools(&mut world);
        // The file body landed in the HashMap region.
        assert_eq!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("files")
                .unwrap()
                .content
                .len(),
            1
        );
    }

    // ── requires_children gate (#7) ──

    use crate::components::SubAgentChildren;

    fn state_with(status: AgentStatus) -> AgentState {
        AgentState {
            status,
            ..agent_state()
        }
    }

    fn requires_children_bp(req: bool) -> AgentBlueprint {
        let mut s = stage_named("a", None, false, None);
        s.requires_children = req;
        AgentBlueprint(blueprint(vec![s]))
    }

    fn children(entities: Vec<Entity>) -> SubAgentChildren {
        SubAgentChildren {
            children: entities,
            max_child_depth: 3,
        }
    }

    fn run_gate_children(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(gate_requires_children);
        s.run(world);
    }

    #[test]
    fn is_terminal_status_classifies_all_variants() {
        assert!(is_terminal_status(&AgentStatus::Complete));
        assert!(is_terminal_status(&AgentStatus::Error {
            message: "x".to_string()
        }));
        assert!(is_terminal_status(&AgentStatus::Cancelled));
        assert!(!is_terminal_status(&AgentStatus::Active));
        assert!(!is_terminal_status(&AgentStatus::Idle));
        assert!(!is_terminal_status(&AgentStatus::Waiting));
    }

    #[test]
    fn gate_requires_children_holds_then_resumes() {
        let mut world = World::new();
        let child = world.spawn(state_with(AgentStatus::Active)).id();
        let parent = world
            .spawn((
                requires_children_bp(true),
                StageCursor { index: 0 },
                agent_state(),
                children(vec![child]),
                ResolveTransition,
            ))
            .id();
        run_gate_children(&mut world);
        assert!(world.get::<WaitingForChildren>(parent).is_some());
        assert!(world.get::<ResolveTransition>(parent).is_none());
        assert_eq!(
            world.get::<AgentState>(parent).unwrap().status,
            AgentStatus::Waiting
        );

        // Child finishes ⇒ the parent resumes and may transition.
        world.get_mut::<AgentState>(child).unwrap().status = AgentStatus::Complete;
        run_gate_children(&mut world);
        assert!(world.get::<WaitingForChildren>(parent).is_none());
        assert!(world.get::<ResolveTransition>(parent).is_some());
        assert_eq!(
            world.get::<AgentState>(parent).unwrap().status,
            AgentStatus::Active
        );
    }

    #[test]
    fn gate_requires_children_does_not_hold_when_not_required_done_or_absent() {
        let mut world = World::new();
        // requires_children = false, even with a running child ⇒ not held.
        let c1 = world.spawn(state_with(AgentStatus::Active)).id();
        let p_norequire = world
            .spawn((
                requires_children_bp(false),
                StageCursor { index: 0 },
                agent_state(),
                children(vec![c1]),
                ResolveTransition,
            ))
            .id();
        // requires_children = true but the child is already terminal ⇒ not held.
        let c2 = world.spawn(state_with(AgentStatus::Complete)).id();
        let p_done = world
            .spawn((
                requires_children_bp(true),
                StageCursor { index: 0 },
                agent_state(),
                children(vec![c2]),
                ResolveTransition,
            ))
            .id();
        // requires_children = true but the child entity no longer exists ⇒ not held.
        let p_ghost = world
            .spawn((
                requires_children_bp(true),
                StageCursor { index: 0 },
                agent_state(),
                children(vec![Entity::from_raw(999_999)]),
                ResolveTransition,
            ))
            .id();
        run_gate_children(&mut world);
        for p in [p_norequire, p_done, p_ghost] {
            assert!(world.get::<ResolveTransition>(p).is_some());
            assert!(world.get::<WaitingForChildren>(p).is_none());
        }
    }

    #[test]
    fn gate_requires_children_resume_waits_on_pending_and_clears_missing() {
        let mut world = World::new();
        // Held with a still-running child ⇒ stays waiting.
        let child = world.spawn(state_with(AgentStatus::Active)).id();
        let stuck = world
            .spawn((agent_state(), children(vec![child]), WaitingForChildren))
            .id();
        // Held with no children component ⇒ resumes (vacuously done).
        let bare = world.spawn((agent_state(), WaitingForChildren)).id();
        // Held with a missing child entity ⇒ resumes.
        let ghost = world
            .spawn((
                agent_state(),
                children(vec![Entity::from_raw(999_999)]),
                WaitingForChildren,
            ))
            .id();
        run_gate_children(&mut world);
        assert!(world.get::<WaitingForChildren>(stuck).is_some());
        assert!(world.get::<ResolveTransition>(stuck).is_none());
        for p in [bare, ghost] {
            assert!(world.get::<WaitingForChildren>(p).is_none());
            assert!(world.get::<ResolveTransition>(p).is_some());
        }
    }

    fn world_with_compaction_results() -> (World, mpsc::UnboundedSender<CompactionOutcome>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(CompactionResults(rx));
        (world, tx)
    }

    fn run_collect_compaction(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(collect_compaction);
        s.run(world);
    }

    #[test]
    fn collect_compaction_stores_summary_and_clears_source() {
        let (mut world, tx) = world_with_compaction_results();
        let e = world.spawn((compacting_window(), AwaitingCompaction)).id();
        tx.send(CompactionOutcome {
            entity: e,
            result: Ok(vec![("conv".to_string(), "the summary".to_string())]),
        })
        .unwrap();

        run_collect_compaction(&mut world);

        let w = world.get::<ContextWindow>(e).unwrap();
        assert_eq!(w.get_region("conv").unwrap().current_tokens, 0); // source cleared
        assert!(w.get_region("history").unwrap().current_tokens > 0); // summary stored
        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<AwaitingCompaction>(e).is_none());
    }

    #[test]
    fn collect_compaction_error_leaves_context_and_readies() {
        let (mut world, tx) = world_with_compaction_results();
        let e = world.spawn((compacting_window(), AwaitingCompaction)).id();
        let before = world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("conv")
            .unwrap()
            .current_tokens;
        tx.send(CompactionOutcome {
            entity: e,
            result: Err(leviath_providers::ProviderError::Other("boom".to_string())),
        })
        .unwrap();

        run_collect_compaction(&mut world);

        // Context untouched on failure, but the agent proceeds.
        assert_eq!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("conv")
                .unwrap()
                .current_tokens,
            before
        );
        assert!(world.get::<ReadyToInfer>(e).is_some());
    }

    #[test]
    fn collect_compaction_drops_stale_outcome() {
        let (mut world, tx) = world_with_compaction_results();
        let ghost = world.spawn_empty().id();
        tx.send(CompactionOutcome {
            entity: ghost,
            result: Ok(vec![]),
        })
        .unwrap();
        run_collect_compaction(&mut world); // no matching agent ⇒ dropped
    }

    #[test]
    fn collect_compaction_summary_for_unpaired_region_is_skipped() {
        // A summary for a region with no paired CompactHistory still clears the
        // source (exercises the None history branch).
        let (mut world, tx) = world_with_compaction_results();
        let mut w = ContextWindow::new(100);
        let mut lone = Region::new(
            "lone".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 5,
            },
            100,
        );
        let _ = lone.add_entry("z".repeat(80), 20);
        w.add_region(lone);
        w.current_tokens = w.calculate_tokens();
        let e = world.spawn((w, AwaitingCompaction)).id();
        tx.send(CompactionOutcome {
            entity: e,
            // "lone" exists but is unpaired (history None); "gone" doesn't exist
            // at all (get_region_mut None) — both no-op branches.
            result: Ok(vec![
                ("lone".to_string(), "s".to_string()),
                ("gone".to_string(), "s2".to_string()),
            ]),
        })
        .unwrap();

        run_collect_compaction(&mut world);

        assert_eq!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("lone")
                .unwrap()
                .current_tokens,
            0
        );
    }

    // ── persistence dispatch ──

    fn run_metadata() -> RunMetadata {
        RunMetadata {
            run_id: "run-1".to_string(),
            agent_name: "a".to_string(),
            agent_path: "/p".to_string(),
            task: "t".to_string(),
            model: None,
            workdir: "/w".to_string(),
            num_stages: 1,
            started_at: 0,
            parent_run_id: None,
            metadata: std::collections::HashMap::new(),
            callback_url: None,
            title: None,
        }
    }

    fn world_with_persistence() -> (World, mpsc::UnboundedReceiver<PersistJob>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(PersistenceStage(tx));
        (world, rx)
    }

    fn run_dispatch_persistence(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(dispatch_persistence);
        s.run(world);
    }

    // ── interaction-status reflection ──

    fn run_reflect(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(reflect_interaction_status);
        s.run(world);
    }

    fn reflect_state(id: &str, status: AgentStatus) -> AgentState {
        AgentState {
            agent_id: id.to_string(),
            status,
            ..agent_state()
        }
    }

    /// Register an open request for `agent_id` and wait for it to land in the
    /// hub. Returns the join handle for the still-awaiting `ask` so the caller
    /// can drop it at the end.
    async fn open_request(
        hub: &InteractionHub,
        agent_id: &str,
        request_id: &str,
    ) -> tokio::task::JoinHandle<leviath_core::interaction::InteractionResponse> {
        use crate::dynamic_interaction::InteractionBackend;
        let backend = hub.backend_for(agent_id.to_string());
        let rid = request_id.to_string();
        let handle = tokio::spawn(async move {
            backend
                .ask(leviath_core::interaction::InteractionRequest::free_text(
                    rid, "p", "s", true,
                ))
                .await
        });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        handle
    }

    #[tokio::test]
    async fn reflect_flips_active_to_waiting_and_back_when_prompt_clears() {
        let hub = InteractionHub::new();
        let asking = open_request(&hub, "a", "q1").await;

        let mut world = World::new();
        world.insert_resource(hub.clone());
        let e = world.spawn(reflect_state("a", AgentStatus::Active)).id();

        // Open prompt ⇒ Active → Waiting, tagged AwaitingInteraction.
        run_reflect(&mut world);
        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Waiting
        );
        assert!(world.get::<AwaitingInteraction>(e).is_some());

        // Still pending, already marked ⇒ no-op (the `(true, true)` arm).
        run_reflect(&mut world);
        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Waiting
        );

        // Answered ⇒ Waiting → Active, marker removed.
        assert!(
            hub.answer(leviath_core::interaction::InteractionResponse::text(
                "q1", "ok"
            ))
        );
        run_reflect(&mut world);
        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Active
        );
        assert!(world.get::<AwaitingInteraction>(e).is_none());

        // No pending, no marker ⇒ no-op (the `(false, false)` arm).
        run_reflect(&mut world);
        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Active
        );
        let _ = asking.await;
    }

    #[tokio::test]
    async fn reflect_does_not_flip_a_non_active_agent_with_an_open_prompt() {
        // A terminal agent that happens to still have an open hub entry is left
        // as-is (the inner `status == Active` guard) — no spurious Waiting.
        let hub = InteractionHub::new();
        let asking = open_request(&hub, "a", "q1").await;

        let mut world = World::new();
        world.insert_resource(hub.clone());
        let e = world.spawn(reflect_state("a", AgentStatus::Complete)).id();

        run_reflect(&mut world);
        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Complete
        );
        assert!(world.get::<AwaitingInteraction>(e).is_none());
        hub.cancel("q1");
        let _ = asking.await;
    }

    #[test]
    fn reflect_clears_a_stale_marker_without_reviving_a_terminal_agent() {
        // Marker present, request gone, but the agent has since gone terminal:
        // remove the marker but leave the terminal status untouched (the
        // `status == Waiting` guard on the restore path).
        let hub = InteractionHub::new(); // empty ⇒ nothing pending
        let mut world = World::new();
        world.insert_resource(hub);
        let e = world
            .spawn((
                reflect_state("a", AgentStatus::Cancelled),
                AwaitingInteraction,
            ))
            .id();

        run_reflect(&mut world);
        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Cancelled
        );
        assert!(world.get::<AwaitingInteraction>(e).is_none());
    }

    #[test]
    fn reflect_is_a_noop_without_a_hub_resource() {
        // Test worlds don't install the hub; the system must not panic and must
        // leave agents untouched.
        let mut world = World::new();
        let e = world.spawn(reflect_state("a", AgentStatus::Active)).id();
        run_reflect(&mut world);
        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Active
        );
        assert!(world.get::<AwaitingInteraction>(e).is_none());
    }

    fn spawn_persistable(world: &mut World) -> Entity {
        world
            .spawn((
                run_metadata(),
                agent_state(),
                conv_window(),
                StageCursor { index: 0 },
                TokenTotals::default(),
                PersistWatermark::default(),
            ))
            .id()
    }

    #[test]
    fn persistence_writes_on_first_dispatch_then_debounces() {
        let (mut world, mut rx) = world_with_persistence();
        let _e = spawn_persistable(&mut world);

        run_dispatch_persistence(&mut world);
        let job = rx.try_recv().expect("first snapshot written");
        assert_eq!(job.run_id, "run-1");

        // No change ⇒ no second write.
        run_dispatch_persistence(&mut world);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn persistence_rewrites_when_iteration_changes() {
        let (mut world, mut rx) = world_with_persistence();
        let e = spawn_persistable(&mut world);

        run_dispatch_persistence(&mut world);
        let _ = rx.try_recv().expect("first snapshot");

        world.get_mut::<AgentState>(e).unwrap().iteration += 1;
        run_dispatch_persistence(&mut world);
        let job = rx.try_recv().expect("second snapshot after change");
        assert_eq!(job.meta.iteration, 1);
    }

    #[test]
    fn persistence_rewrites_when_status_changes() {
        let (mut world, mut rx) = world_with_persistence();
        let e = spawn_persistable(&mut world);
        run_dispatch_persistence(&mut world);
        let _ = rx.try_recv().expect("first snapshot");

        world.get_mut::<AgentState>(e).unwrap().status = AgentStatus::Complete;
        run_dispatch_persistence(&mut world);
        let job = rx.try_recv().expect("snapshot after completion");
        assert_eq!(job.meta.status, leviath_core::run_meta::RunStatus::Complete);
    }

    // ── async LLM-choice transition ──

    fn plain_edge(target: &str) -> leviath_core::blueprint::TransitionEdge {
        leviath_core::blueprint::TransitionEdge {
            target: target.to_string(),
            condition: leviath_core::blueprint::TransitionCondition::LlmChoice,
            hint: None,
            transform: leviath_core::blueprint::EdgeTransform::Direct,
        }
    }

    #[test]
    fn match_choice_done_completes_when_allowed() {
        let edges = vec![plain_edge("b")];
        assert_eq!(match_transition_choice("DONE", &edges, true), None);
        // Not allowed to complete ⇒ "done" is just text ⇒ falls back to first edge.
        assert_eq!(
            match_transition_choice("done", &edges, false),
            Some("b".to_string())
        );
    }

    #[test]
    fn match_choice_exact_and_contains_and_fallback() {
        let edges = vec![plain_edge("review"), plain_edge("plan")];
        // Exact (case-insensitive).
        assert_eq!(
            match_transition_choice("REVIEW", &edges, false),
            Some("review".to_string())
        );
        // Substring (choice contains the target verbatim).
        assert_eq!(
            match_transition_choice("go to plan now", &edges, false),
            Some("plan".to_string())
        );
        // Lowercase-contains fallback (target casing differs from the response).
        let mixed = vec![plain_edge("Deploy")];
        assert_eq!(
            match_transition_choice("please deploy it", &mixed, false),
            Some("Deploy".to_string())
        );
        // No match at all ⇒ first edge.
        assert_eq!(
            match_transition_choice("nonsense", &edges, false),
            Some("review".to_string())
        );
        // No edges ⇒ nothing to pick.
        assert_eq!(match_transition_choice("x", &[], false), None);
    }

    #[test]
    fn build_transition_prompt_default_variants() {
        let mut with_complete = stage_named("s", None, true, None);
        with_complete.transition_prompt = None;
        let edges = vec![{
            let mut e = plain_edge("next");
            e.hint = Some("go next".to_string());
            e
        }];
        let p = build_transition_prompt(&with_complete, &edges);
        assert!(p.contains("Stage 's' is complete"));
        assert!(p.contains("- next: go next")); // hint rendered
        assert!(p.contains("DONE")); // allow_complete branch

        let no_complete = stage_named("s", None, false, None);
        let p2 = build_transition_prompt(&no_complete, &edges);
        assert!(!p2.contains("DONE"));
        assert!(p2.contains("ONLY the stage name"));
    }

    #[test]
    fn build_transition_prompt_custom_variants() {
        let mut custom = stage_named("s", None, true, None);
        custom.transition_prompt = Some("Pick wisely.".to_string());
        let edges = vec![plain_edge("a")];
        let p = build_transition_prompt(&custom, &edges);
        assert!(p.starts_with("Pick wisely."));
        assert!(p.contains("Available transitions:"));
        assert!(p.contains("DONE"));

        custom.allow_complete = false;
        let p2 = build_transition_prompt(&custom, &edges);
        assert!(!p2.contains("DONE"));
        assert!(p2.contains("nothing else"));
    }

    fn conv_window() -> ContextWindow {
        let mut w = ContextWindow::new(10_000);
        w.add_region(Region::new(
            "conversation".to_string(),
            RegionKind::Clearable,
            10_000,
        ));
        w
    }

    fn spawn_choosing_agent(
        world: &mut World,
        bp: leviath_core::Blueprint,
        stage_infs: Vec<StageInference>,
        edges: Vec<leviath_core::blueprint::TransitionEdge>,
    ) -> Entity {
        world
            .spawn((
                AgentBlueprint(bp),
                StageCursor { index: 0 },
                agent_state(),
                StageProgress::default(),
                StageInferences(stage_infs),
                VisitCounts::default(),
                conv_window(),
                stage_infs_head(),
                AwaitingTransitionChoice(edges),
            ))
            .id()
    }

    // The choosing agent also carries its current `StageInference` (dispatch reads
    // provider/model off it).
    fn stage_infs_head() -> StageInference {
        StageInference {
            provider_name: "cfg".to_string(),
            model: "m".to_string(),
            tools: vec![],
            tool_filter: None,
        }
    }

    #[tokio::test]
    async fn dispatch_choice_moves_to_awaiting_response_and_injects_prompt() {
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let (ttx, mut trx) = mpsc::unbounded_channel();
        world.resource_mut::<InferenceStage>().transition_outcomes = ttx;

        let bp = blueprint(vec![
            stage_named("a", None, false, None),
            stage_named("b", None, false, None),
        ]);
        let e = spawn_choosing_agent(
            &mut world,
            bp,
            vec![si("m0"), si("m1")],
            vec![plain_edge("b")],
        );

        let mut schedule = Schedule::default();
        schedule.add_systems(dispatch_transition_choice);
        schedule.run(&mut world);

        assert!(world.get::<AwaitingTransitionResponse>(e).is_some());
        assert!(world.get::<AwaitingTransitionChoice>(e).is_none());
        // Prompt injected into the conversation region.
        assert!(
            world
                .get::<ContextWindow>(e)
                .unwrap()
                .get_region("conversation")
                .unwrap()
                .current_tokens
                > 0
        );
        // The spawned routing job reports back on the transition lane.
        let outcome = trx.recv().await.expect("routing outcome");
        assert_eq!(outcome.entity, e);
    }

    #[tokio::test]
    async fn dispatch_choice_skips_non_active_agent() {
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let bp = blueprint(vec![
            stage_named("a", None, false, None),
            stage_named("b", None, false, None),
        ]);
        let e = spawn_choosing_agent(
            &mut world,
            bp,
            vec![si("m0"), si("m1")],
            vec![plain_edge("b")],
        );
        world.get_mut::<AgentState>(e).unwrap().status = AgentStatus::Cancelled;

        let mut schedule = Schedule::default();
        schedule.add_systems(dispatch_transition_choice);
        schedule.run(&mut world);

        assert!(world.get::<AwaitingTransitionChoice>(e).is_some()); // stayed
        assert!(world.get::<AwaitingTransitionResponse>(e).is_none());
    }

    #[tokio::test]
    async fn dispatch_choice_stays_when_provider_missing() {
        let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
        let bp = blueprint(vec![stage_named("a", None, false, None)]);
        let mut infs = vec![si("m0")];
        infs[0].provider_name = "ghost".to_string();
        let e = spawn_choosing_agent(&mut world, bp, infs, vec![plain_edge("a")]);
        // Override the head StageInference to the missing provider too.
        world.entity_mut(e).insert(StageInference {
            provider_name: "ghost".to_string(),
            model: "m".to_string(),
            tools: vec![],
            tool_filter: None,
        });

        let mut schedule = Schedule::default();
        schedule.add_systems(dispatch_transition_choice);
        schedule.run(&mut world);

        assert!(world.get::<AwaitingTransitionChoice>(e).is_some()); // stayed
    }

    #[tokio::test]
    async fn dispatch_choice_stays_when_pool_full() {
        let mut cfg = InferencePoolConfig::new();
        cfg.set_limit("m", 0); // no permits for model "m"
        let (mut world, _rx) = build_world(InferencePools::new(cfg));
        let bp = blueprint(vec![stage_named("a", None, false, None)]);
        let e = spawn_choosing_agent(&mut world, bp, vec![si("m0")], vec![plain_edge("a")]);

        let mut schedule = Schedule::default();
        schedule.add_systems(dispatch_transition_choice);
        schedule.run(&mut world);

        assert!(world.get::<AwaitingTransitionChoice>(e).is_some()); // stayed
    }

    fn world_with_transition_results() -> (World, mpsc::UnboundedSender<InferenceOutcome>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut world = World::new();
        world.insert_resource(TransitionResults(rx));
        (world, tx)
    }

    fn spawn_responding_agent(
        world: &mut World,
        bp: leviath_core::Blueprint,
        stage_infs: Vec<StageInference>,
        edges: Vec<leviath_core::blueprint::TransitionEdge>,
    ) -> Entity {
        let n = stage_infs.len();
        world
            .spawn((
                AgentBlueprint(bp),
                StageCursor { index: 0 },
                agent_state(),
                StageProgress::default(),
                StageInferences(stage_infs),
                setups(n),
                VisitCounts::default(),
                conv_window(),
                AwaitingTransitionResponse(edges),
            ))
            .id()
    }

    fn run_collect_transition(world: &mut World) {
        let mut s = Schedule::default();
        s.add_systems(collect_transition_choice);
        s.run(world);
    }

    #[test]
    fn collect_choice_enters_chosen_stage() {
        let (mut world, tx) = world_with_transition_results();
        let bp = blueprint(vec![
            stage_named("a", None, false, None),
            stage_named("b", None, false, None),
        ]);
        let e = spawn_responding_agent(
            &mut world,
            bp,
            vec![si("m0"), si("m1")],
            vec![plain_edge("b")],
        );
        tx.send(InferenceOutcome {
            entity: e,
            result: Ok(resp("b")),
        })
        .unwrap();

        run_collect_transition(&mut world);

        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
        assert_eq!(world.get::<StageInference>(e).unwrap().model, "m1");
        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<AwaitingTransitionResponse>(e).is_none());
        assert_eq!(world.get::<AgentState>(e).unwrap().current_stage, "b");
    }

    #[test]
    fn collect_choice_applies_the_chosen_edge_transform() {
        let (mut world, tx) = world_with_transition_results();
        let bp = blueprint(vec![
            stage_named("a", None, false, None),
            stage_named("b", None, false, None),
        ]);
        let mut edge = plain_edge("b");
        edge.transform = EdgeTransform::Compact { prompt: None };
        let e = spawn_responding_agent(&mut world, bp, vec![si("m0"), si("m1")], vec![edge]);
        world
            .get_mut::<ContextWindow>(e)
            .unwrap()
            .add_to_region("conversation", "summarize me".to_string(), 10)
            .unwrap();
        tx.send(InferenceOutcome {
            entity: e,
            result: Ok(resp("b")),
        })
        .unwrap();

        run_collect_transition(&mut world);

        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
        // The chosen edge's Compact transform queued the conversation region.
        assert_eq!(
            world.get::<PendingEdgeCompact>(e).unwrap().0,
            vec!["conversation".to_string()]
        );
    }

    #[test]
    fn collect_choice_done_completes() {
        let (mut world, tx) = world_with_transition_results();
        let bp = blueprint(vec![stage_named("a", None, true, None)]); // allow_complete
        let e = spawn_responding_agent(&mut world, bp, vec![si("m0")], vec![plain_edge("a")]);
        tx.send(InferenceOutcome {
            entity: e,
            result: Ok(resp("DONE")),
        })
        .unwrap();

        run_collect_transition(&mut world);

        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Complete
        );
        assert!(world.get::<ReadyToInfer>(e).is_none());
    }

    #[test]
    fn collect_choice_unknown_target_falls_back_to_first_stage() {
        let (mut world, tx) = world_with_transition_results();
        // Edge target "b" exists as a stage; the LLM names it, so idx resolves. To
        // exercise the position()-unwrap_or(0) fallback we point the edge at a
        // name that survives matching but isn't a stage.
        let bp = blueprint(vec![stage_named("a", None, false, None)]);
        let e = spawn_responding_agent(&mut world, bp, vec![si("m0")], vec![plain_edge("ghost")]);
        tx.send(InferenceOutcome {
            entity: e,
            result: Ok(resp("ghost")),
        })
        .unwrap();

        run_collect_transition(&mut world);

        // Matched "ghost" but no such stage ⇒ idx 0 ⇒ re-enters stage "a".
        assert_eq!(world.get::<StageCursor>(e).unwrap().index, 0);
        assert!(world.get::<ReadyToInfer>(e).is_some());
    }

    #[test]
    fn collect_choice_marks_error_on_failure() {
        let (mut world, tx) = world_with_transition_results();
        let bp = blueprint(vec![stage_named("a", None, false, None)]);
        let e = spawn_responding_agent(&mut world, bp, vec![si("m0")], vec![plain_edge("a")]);
        tx.send(InferenceOutcome {
            entity: e,
            result: Err(leviath_providers::ProviderError::Other("boom".to_string())),
        })
        .unwrap();

        run_collect_transition(&mut world);

        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Error {
                message: "boom".to_string()
            }
        );
        assert!(world.get::<AwaitingTransitionResponse>(e).is_none());
    }

    #[test]
    fn collect_choice_drops_stale_outcome() {
        let (mut world, tx) = world_with_transition_results();
        let ghost = world.spawn_empty().id();
        tx.send(InferenceOutcome {
            entity: ghost,
            result: Ok(resp("x")),
        })
        .unwrap();
        // No matching AwaitingTransitionResponse agent ⇒ silently dropped.
        run_collect_transition(&mut world);
    }
}
