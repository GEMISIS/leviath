//! Tool dispatch: batching, policy triage, and handing calls to the tool lane.

use super::*;

/// The agent's tool batch has been handed to the tool lane; it is waiting for
/// the results (which the tool-collect system will apply).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AwaitingTools;

/// Marker: this agent's advertised tools should be re-resolved before its next
/// turn - mid-run dynamic tool discovery. Consumed by
/// [`refresh_advertised_tools`], which asks the [`ToolService`] for the stage's
/// fresh tool defs and writes them into the live [`StageInference`].
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct ToolsNeedRefresh;

/// Marker: this agent's blueprint asks for tools to be looked for again after
/// the run starts. Only such agents are polled by `poll_dynamic_tool_refresh`
/// for a pending tool re-scan, so the default (fixed) agent pays nothing.
#[derive(Component, Debug, Clone, Copy)]
pub struct DynamicTools;

/// Marker: this agent's blueprint asks for the scanned directories to be looked
/// at before *each* batch of tool calls, on top of the refresh before its next
/// turn.
///
/// What it buys is noticing a tool nobody told the service about. The
/// between-turns refresh fires on a dirty flag this agent's own `write_file`,
/// `edit_file` or `install_tool` sets; a tool written by a shell command, by a
/// script tool, or by another agent sharing the workdir sets nothing. Read by
/// `rescan_before_dispatch` only, and its cost is a `stat` per scanned
/// directory per batch.
#[derive(Component, Debug, Clone, Copy)]
pub struct RescanBeforeDispatch;

/// Reports one tool call's result the moment it resolves, from inside the
/// executor - `(tool_call_id, result)`. Dispatch builds one per batch to journal
/// each completion as a `ToolCallDone` record, so a crash mid-batch loses only
/// the calls that genuinely never finished. Implementors that don't
/// journal get a no-op.
pub type ToolProgress = Arc<dyn Fn(&str, &leviath_core::region::EntryContent) + Send + Sync>;

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

    /// Hand the service the stored parts `entity`'s window holds, right
    /// before a batch is dispatched, so a tool that takes a part by name can
    /// find it off the tick. Called with the whole current list each time;
    /// an empty list means the window holds none. Default no-op for services
    /// whose tools take no parts.
    fn offer_parts(&self, _entity: Entity, _parts: Vec<leviath_core::mime::Part>) {}

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

    /// Whether `entity` (an agent that rescans) has pending tool changes that
    /// warrant a re-scan + re-advertise. Polled by `poll_dynamic_tool_refresh`;
    /// implementors return (and clear) a per-agent dirty flag. Default `false`.
    fn wants_refresh(&self, _entity: Entity) -> bool {
        false
    }

    /// Whether `entity`'s scanned directories have changed since they were last
    /// read, asked once per batch of a `before_dispatch` agent.
    ///
    /// Separate from [`wants_refresh`](Self::wants_refresh) because it answers a
    /// cheaper question and must not consume anything: the dirty flag is drained
    /// by the poll that runs before the turn, and this runs in the middle of
    /// one. Implementors compare a stamp of the directories rather than
    /// re-reading them, so the common answer costs a few `stat` calls.
    /// Default `false`, which turns the mode off for a service without one.
    fn scan_stale(&self, _entity: Entity) -> bool {
        false
    }

    /// Decide `call` for `entity` before its batch runs. Called in the world,
    /// once per call, in the order the model made them; `ctx` is what the run
    /// has been granted and what it has written so far. Default: run it,
    /// charging nothing, for a service that applies no policy.
    fn decide(
        &self,
        _entity: Entity,
        _call: &leviath_providers::ToolCall,
        _ctx: &super::tool_verdicts::DecideCtx<'_>,
    ) -> super::tool_verdicts::ToolVerdict {
        super::tool_verdicts::ToolVerdict::Run { charge: 0 }
    }

    /// Build the closure that runs a decided batch: each call with what the
    /// world decided for it. `written` is the run's write total as the batch
    /// starts. Default: run every call through [`exec_for`](Self::exec_for),
    /// which is all a service that never refuses or asks needs.
    fn exec_decided(
        &self,
        entity: Entity,
        calls: Vec<super::tool_verdicts::DecidedCall>,
        _written: u64,
        progress: ToolProgress,
    ) -> BoxedToolExec {
        self.exec_for(
            entity,
            calls.into_iter().map(|d| d.call).collect(),
            progress,
        )
    }

    /// What `entity`'s run has written by the end of its last batch, as its
    /// executors measured it. `None` for a service that keeps no write total.
    fn written(&self, _entity: Entity) -> Option<u64> {
        None
    }
}

/// The tool service, as a world resource.
#[derive(Resource, Clone)]
pub(crate) struct ToolServiceRes(pub Arc<dyn ToolService>);

/// The job sender feeding the tool lane, as a world resource, paired with the
/// lane's occupancy counters so dispatch can record what it queued.
#[derive(Resource, Clone)]
pub(crate) struct ToolStage {
    /// Where batches are handed to the lane.
    pub jobs: UnboundedSender<ToolJob>,
    /// Shared with the lane's workers; see [`crate::tool_bridge::ToolLaneStats`].
    pub stats: Arc<crate::tool_bridge::ToolLaneStats>,
}

impl ToolStage {
    /// A stage wired to a real lane's counters.
    pub(crate) fn new(
        jobs: UnboundedSender<ToolJob>,
        stats: Arc<crate::tool_bridge::ToolLaneStats>,
    ) -> Self {
        Self { jobs, stats }
    }

    /// A stage with counters of its own, for callers that drive `dispatch_tools`
    /// without a lane behind it (tests read the channel directly).
    #[cfg(test)]
    pub(crate) fn detached(jobs: UnboundedSender<ToolJob>) -> Self {
        Self::new(jobs, Arc::new(crate::tool_bridge::ToolLaneStats::new(1)))
    }
}

/// Context-tool results computed inline by [`dispatch_tools`] (the `context_*`
/// tools mutate the ECS window, so they can't run on the async lane), held until
/// [`collect_tools`] merges them with the lane results. Absent when a batch had
/// no context tools.
#[derive(Component, Debug, Clone, Default)]
pub(crate) struct ContextToolResults(pub Vec<(String, String)>);

/// Results a batch already had before a restart, carried into its re-dispatch.
///
/// The calls that finished before the daemon stopped are here with the result
/// the run's file recorded, and so is the stand-in for each call a stop
/// interrupted (see `restore::interrupt_in_flight`), so [`dispatch_tools`] runs
/// only the rest and none of these twice. A batch stopped on a question to a
/// person has no stand-ins: nothing in it ran after the question, so it is
/// asked again. Held until [`super::collect_tools`] merges them, or until an
/// all-inline batch applies them.
#[derive(Component, Debug, Clone, Default)]
pub(crate) struct RecoveredResults(pub Vec<crate::tool_bridge::ToolResult>);

/// The execution each call of a batch brought back from a run's file was
/// dispatched as, by call id. Its presence says the file already records the
/// batch as dispatched, so [`dispatch_tools`] keeps these ids and records only
/// what is new: the results it settles now, and the calls it sends to the
/// lane again.
#[derive(Component, Debug, Clone, Default)]
pub(crate) struct ResumedExecutions(pub std::collections::HashMap<String, String>);

/// The results of a batch's lane calls that have landed while the rest still
/// run, written by the batch's [`ToolProgress`] the moment each call resolves.
/// Read by `inspect`, so the run's state (and the run file it is recorded in)
/// holds a finished call as done before the batch ends.
#[derive(Component, Debug, Clone, Default)]
pub(crate) struct LandedResults(pub Arc<std::sync::Mutex<Vec<crate::tool_bridge::ToolResult>>>);

impl LandedResults {
    /// What has landed so far.
    pub(crate) fn snapshot(&self) -> Vec<crate::tool_bridge::ToolResult> {
        self.0
            .lock()
            .expect("the landed results are never held across a panic")
            .clone()
    }

    /// `progress`, also keeping each result here as it lands.
    pub(super) fn keeping(&self, progress: ToolProgress) -> ToolProgress {
        let landed = self.0.clone();
        Arc::new(move |call_id, result| {
            landed
                .lock()
                .expect("the landed results are never held across a panic")
                .push((call_id.to_string(), result.clone()));
            progress(call_id, result);
        })
    }
}

/// Merge context + lane tool results into one `(id, result)` list in the
/// original tool-call order (Anthropic requires a `tool_result` per `tool_use`,
/// in order).
/// Inline results, which are always text, in the shape the lane's carry.
pub(crate) fn typed_results(results: &[(String, String)]) -> Vec<crate::tool_bridge::ToolResult> {
    results
        .iter()
        .map(|(id, text)| (id.clone(), text.clone().into()))
        .collect()
}

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
    parts: &[crate::tool_bridge::ToolResult],
) -> Vec<crate::tool_bridge::ToolResult> {
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
/// count it as work. One predicate, one list: keep the prefixes in two places
/// and adding a fourth means a call site still testing three, which files a
/// blocked write as a modification.
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
/// journal-record ack. Every outcome - landed, no journal, failed, timed out or
/// a dropped sender - proceeds to run the batch.
///
/// What changes with the outcome is what gets said about it. A batch that runs
/// after its record failed to land has side effects the journal does not
/// mention, and a run debugger reading that journal would show the batch as
/// never dispatched. That is worth a line in the log, and telling it apart from
/// a world that keeps no journal at all is why the ack carries a state rather
/// than a signal.
pub(crate) fn barrier_then(
    exec: BoxedToolExec,
    ack: tokio::sync::oneshot::Receiver<crate::persistence_bridge::Appended>,
    timeout: std::time::Duration,
    run_id: String,
) -> BoxedToolExec {
    use crate::persistence_bridge::Appended;
    Box::new(move || {
        Box::pin(async move {
            match tokio::time::timeout(timeout, ack).await {
                Ok(Ok(Appended::Landed { position })) => {
                    tracing::debug!(run_id = %run_id, position, "tool batch record landed");
                }
                // No journal to land in, which is what an in-memory world is.
                Ok(Ok(Appended::NoJournal)) => {}
                Ok(Ok(Appended::Failed)) => {
                    tracing::warn!(
                        run_id = %run_id,
                        "tool batch runs with no record of it: the journal append failed"
                    );
                }
                // A dropped sender is the lane shutting down mid-dispatch.
                Ok(Err(_)) => {
                    tracing::debug!(run_id = %run_id, "tool batch record abandoned by the lane");
                }
                Err(_) => {
                    tracing::warn!(
                        run_id = %run_id,
                        "tool batch dispatched before its record: the journal lane is behind"
                    );
                }
            }
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
        // Only for the all-inline batch below, which returns before
        // `collect_tools` (the usual writer of `[tool]` lines) ever sees it.
        Option<&'static mut crate::pipeline::response::StageIoBuffer>,
    ),
    Option<&'static crate::components::OutputValidators>,
    // For the submit_output guard: a submission that is exactly the name of a
    // stage in this graph is a routing token, not an answer.
    Option<&'static crate::insert::RunSpecC>,
);

/// The resources the daemon installs, which a bare world does not have.
///
/// Every field is optional because `lev run` drives these same systems with no
/// daemon behind them: no gate lane to prompt through, no persistence lane, no
/// event sink. Bundled as one `SystemParam` so a system's signature stays about
/// what it *queries* rather than listing the six things that might be wired.
#[derive(bevy_ecs::system::SystemParam)]
pub(crate) struct DaemonServices<'w> {
    /// The taint-gate policy, when one is configured.
    pub policy: Option<Res<'w, PolicyGate>>,
    /// Rhai rules the gate consults before blocking.
    pub script_rules: Option<Res<'w, GateScriptRules>>,
    /// The hub a blocked call can prompt through.
    pub hub: Option<Res<'w, InteractionHub>>,
    /// The lane a gate prompt's answer comes back on.
    pub gate_stage: Option<Res<'w, crate::gate_prompt::GatePromptStage>>,
    /// Where what a run does is recorded, for its run file.
    pub persist: Option<Res<'w, super::JournalSender>>,
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
/// (inline results pre-filled, lane calls pending) goes to the world's journal
/// with an ack the exec waits on, answered once the step holding it is written,
/// and a per-call [`ToolProgress`] journals each completion as a
/// `ToolCallDone`. On a crash mid-batch, recovery replays the
/// recorded results instead of re-running their side effects.
pub(crate) fn dispatch_tools(
    mut agents: Query<DispatchToolsQuery, With<ReadyForTools>>,
    carried: Query<(
        Option<&RecoveredResults>,
        Option<&ResumedExecutions>,
        Option<&super::lane_batch::ResumedHold>,
    )>,
    daemon: DaemonServices,
    mime: crate::blob_store::MimeParams,
    mut commands: Commands,
) {
    let DaemonServices {
        policy,
        script_rules,
        hub,
        gate_stage,
        persist,
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
        cursor,
        (metadata, stage_progress, mut io_buffer),
        validators,
        blueprint,
    ) in agents.iter_mut()
    {
        crate::tick_scope::enter(entity);
        // Where this run's produced files and oversized text are stored.
        let (sources, _) = mime.hydration_inputs(entity);
        let part_sink = crate::context_setup::PartSink::over(&sources, &state.agent_id, &mime);
        // `--yolo`: waive taint-gate enforcement so a headless run never blocks
        // on a gate prompt no one can answer (taint tracking still records).
        let auto_approve_gates = auto_gate.is_some();
        let (recovered, resumed, hold) = carried.get(entity).unwrap_or_default();
        // Paused, waiting or cancelled: no new work starts. A batch a paused
        // run was holding on a person when the daemon stopped is not new
        // work: it puts its questions again, as it was putting them before.
        let reasking = state.status == AgentStatus::Paused && hold.is_some();
        if state.status != AgentStatus::Active && !reasking {
            continue;
        }

        // This stage, for routing the parts a reply produces to regions of
        // their own (`output_routing`). Computed once so both apply paths below
        // share it.
        let routing_stage = blueprint
            .zip(cursor)
            .and_then(|(spec, cur)| spec.0.graph.stages.get(cur.index));

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
        // One execution id per call, minted before anything runs, and kept by
        // a batch brought back from the run's file, whose calls the file
        // already names. The provider's own id travels beside it: a provider
        // may reuse one across a retry, and two attempts under one id cannot be
        // told apart afterwards.
        //
        // Minted ahead of the loop rather than beside the journal write below,
        // because the calls this dispatcher resolves itself need theirs while it
        // is resolving them: a context tool's whole job is to move the window,
        // and the transaction it commits says which execution moved it.
        let executions: std::collections::HashMap<String, String> = result
            .tool_calls
            .iter()
            .map(|c| {
                let kept = resumed.and_then(|r| r.0.get(&c.tool_id).cloned());
                (
                    c.tool_id.clone(),
                    kept.unwrap_or_else(leviath_core::execution::mint_execution_id),
                )
            })
            .collect();
        // Files an accepted submission produced, by the execution that produced
        // them. Journaled after the loop, which is also where the batch record
        // that dispatched them goes.
        let mut produced: Vec<(String, Vec<leviath_core::output::Artifact>)> = Vec::new();
        // What the batch had already finished before a restart. Checked before
        // anything else below, so a context write, a submission or a file tool
        // that already ran is never run again.
        let recovered: Vec<crate::tool_bridge::ToolResult> =
            recovered.map(|r| r.0.clone()).unwrap_or_default();
        for c in &result.tool_calls {
            if recovered.iter().any(|(id, _)| id == &c.tool_id) {
                continue;
            }
            // Everything this call commits to the window is this call's, and
            // nothing after the loop is. Re-set per call, so a change can never
            // be attributed to the call before it.
            window.attribute_to(executions.get(&c.tool_id).map_or("", String::as_str));
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
            // A call whose arguments arrived as text rather than JSON was cut
            // off mid-argument by the output cap (the provider layer keeps
            // the text for exactly this reading). Refused with the cause, so
            // the model shrinks or splits the call instead of repeating it.
            if let serde_json::Value::String(raw) = &c.arguments {
                let in_a_row = stage_progress.map_or(0, |p| p.cut_off_nudges);
                context_results.push((
                    c.tool_id.clone(),
                    cut_off_arguments_refusal(&c.name, raw, in_a_row),
                ));
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
                let stage_max = routing_stage
                    .and_then(|s| s.max_iterations)
                    .map(|n| n as usize);
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
                    unattended: metadata.is_some_and(|m| m.unattended.is_on()),
                    workdir: metadata.map(|m| m.workdir.as_str()),
                };
                let text = crate::runtime_info_tool::handle_runtime_info(&facts, &window);
                context_results.push((c.tool_id.clone(), text));
                continue;
            }
            if crate::mime_tools::is_mime_tool(&c.name) {
                let tool_limit: Option<Vec<String>> = routing_stage
                    .and_then(|s| s.tool_accepts.iter().find(|(t, _)| t.as_str() == c.name))
                    .map(|(_, list)| list.iter().map(ToString::to_string).collect());
                let text = crate::mime_tools::handle_mime_tool(
                    &c.name,
                    &c.arguments,
                    &mut window,
                    &crate::mime_tools::MimeToolContext {
                        mime: &mime,
                        entity,
                        run_id: &state.agent_id,
                        workdir: metadata.map(|m| std::path::Path::new(&m.workdir)),
                        tool_limit: tool_limit.as_deref(),
                    },
                );
                context_results.push((c.tool_id.clone(), text));
                continue;
            }
            if crate::context_tools::is_context_tool(&c.name) {
                let text =
                    crate::context_tools::handle_context_tool(&c.name, &c.arguments, &mut window);
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
                        leviath_core::stage_tools::FAN_OUT_TOOL
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
            // A call the user already resolved in a prior prompt round. An
            // approved one skips the gate and lands where it would have
            // landed the first time: the lane, or the inline submission below.
            let mut cleared = false;
            if let Some(resolved) = resolved {
                if let Some(msg) = resolved.denied.get(&c.tool_id) {
                    context_results.push((c.tool_id.clone(), msg.clone()));
                    continue;
                }
                cleared = resolved.approved.contains(&c.tool_id);
            }
            if let Some(gate) = gate.as_deref_mut()
                && !cleared
            {
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
            // Applied inline for the same reason the context tools are: it
            // writes the live window and an ECS component, neither of which the
            // async lane can reach. Recorded here and committed after the loop,
            // because `commands` cannot be borrowed inside it.
            //
            // After the gate, not before it with the other inline tools: the
            // submitted answer leaves the machine (`GET /api/runs/{id}/result`,
            // the dashboard), so `submit_output` is classified outbound, and a
            // classification the gate never sees gates nothing. Applied above
            // the gate, a Private region reaches a remote reader with no
            // prompt however it was classified.
            if crate::output_tool::is_output_tool(&c.name) {
                let stage_names: Vec<String> = blueprint
                    .map(|spec| {
                        spec.0
                            .graph
                            .stages
                            .iter()
                            .map(|s| s.name.to_string())
                            .collect()
                    })
                    .unwrap_or_default();

                let (text, output) = crate::output_tool::handle_output_tool(
                    &c.arguments,
                    &crate::output_tool::OutputContext {
                        spec: stage_inf.output.as_ref(),
                        validators,
                        stage: &state.current_stage,
                        stage_names: &stage_names,
                        workdir: metadata.map(|m| std::path::Path::new(&m.workdir)),
                        sink: part_sink.as_ref(),
                        overwrite_artifacts: stage_inf
                            .output
                            .as_ref()
                            .and_then(|s| s.overwrite_artifacts)
                            .unwrap_or_else(|| mime.overwrite_artifacts()),
                    },
                    chrono::Utc::now().timestamp(),
                    &mut window,
                );
                // A refused submission leaves any earlier one alone: a bad
                // correction must not erase a good answer.
                if let Some(output) = output {
                    // Recorded against this call while it is still in hand.
                    // `output.json` keeps only the latest answer's files and says
                    // nothing about which call made any of them, so a submission
                    // a later one replaces would otherwise leave no trace.
                    if !output.artifacts.is_empty() {
                        produced.push((
                            executions.get(&c.tool_id).cloned().unwrap_or_default(),
                            output.artifacts.clone(),
                        ));
                    }
                    submitted = Some(output);
                }
                context_results.push((c.tool_id.clone(), text));
                continue;
            }
            lane_calls.push(leviath_providers::ToolCall {
                id: c.tool_id.clone(),
                name: c.name.clone(),
                arguments: c.arguments.clone(),
                thought_signature: c.thought_signature.clone(),
            });
        }
        // Past the last call, so nothing the paths below write is attributed to
        // one: the assistant turn, the routed results and the nudges are the
        // batch's work, not any single call's.
        window.attribute_to("");

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

        // Hold the batch and ask the user about each blocked call. A call
        // asked about before a restart is asked again under the same id.
        if let (false, Some((hub, gate_stage))) = (pending_prompts.is_empty(), interactive) {
            let n = pending_prompts.len();
            let mut held = resolved.cloned().unwrap_or_default();
            for (tool_id, name, taint, clearance) in pending_prompts {
                let question = hold
                    .and_then(|h| h.0.asked.get(&tool_id).cloned())
                    .unwrap_or_else(|| hub.next_request_id(&state.agent_id, "gate"));
                held.asked.insert(tool_id.clone(), question.clone());
                crate::gate_prompt::ask(
                    gate_stage,
                    hub,
                    crate::gate_prompt::GatedCall {
                        entity,
                        agent_id: state.agent_id.clone(),
                        question,
                        tool_id,
                        tool_name: name,
                        taint,
                        clearance,
                    },
                );
            }
            commands
                .entity(entity)
                .remove::<ReadyForTools>()
                .insert(crate::gate_prompt::AwaitingGatePrompt(n))
                .insert(held);
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
                    leviath_core::stage_tools::FAN_OUT_TOOL
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
            let mut resolved = typed_results(&context_results);
            resolved.extend(recovered.iter().cloned());
            let merged: Vec<crate::tool_bridge::ToolResult> =
                merge_in_call_order(&result.tool_calls, &resolved)
                    .into_iter()
                    .filter(|(id, _)| id != &call_id)
                    .collect();
            super::tool_results::apply_tool_results_with_parts(
                &mut window,
                super::tool_results::Reply {
                    text: &result.response,
                    parts: &result.parts,
                    stage: routing_stage,
                    sink: part_sink.as_ref(),
                },
                &result.tool_calls,
                &merged,
                routing.map(|c| &c.routing),
                sensitivities.map(|s| &s.0),
                result.reasoning.clone(),
            );
            commands
                .entity(entity)
                .remove::<ReadyForTools>()
                .remove::<(
                    RecoveredResults,
                    ResumedExecutions,
                    super::lane_batch::ResumedHold,
                )>()
                .insert(crate::fanout::PendingFanOut { call_id, request });
            continue;
        }

        if lane_calls.is_empty() {
            // What the journal is told about this batch. A batch with lane
            // work is journaled by `dispatch_lane_batches`, which holds it
            // while each call is decided.
            let dispatch = super::batch_record::BatchDispatch {
                calls: &result.tool_calls,
                executions: &executions,
                inline: &context_results,
                recovered: &recovered,
                resumed: resumed.is_some(),
                stage_index: cursor.map_or(0, |c| c.index),
                iteration: state.iteration,
                visit_id: &state.current_visit,
                requested_by: &result.attempt_id,
                response: &result.response,
            };
            // Every call resolved without the lane: context tools, refusals,
            // gate denials. Journaled all the same, so the run's executions are
            // every call the model made rather than only the ones something ran
            // asynchronously - and so a transaction a context tool committed
            // names an execution a reader can find. No ack, because nothing is
            // about to run that could outrace the record.
            if let (Some(persist), Some(md)) = (persist.as_ref(), metadata) {
                for record in dispatch.records(&[]) {
                    persist.record(&md.run_id, record);
                }
                super::batch_record::journal_artifacts(persist, &md.run_id, &produced);
            }
            // Nothing async to run - apply the context results now and loop back.
            let mut resolved = typed_results(&context_results);
            resolved.extend(recovered.iter().cloned());
            let merged = merge_in_call_order(&result.tool_calls, &resolved);
            // Log the calls here, because this batch never reaches
            // `collect_tools` - the usual writer of `[tool]` lines - and would
            // otherwise leave no trace anywhere a person can read. A batch of
            // only inline-resolved calls (context tools, a refusal, a gate
            // denial) still counts towards `meta.tool_calls`, so without this
            // a run reports tool calls next to a stage log that recorded none
            // of them, and an empty activity panel reads as a dropped-log bug.
            // A mixed batch is already covered: `collect_tools` zips over
            // every call, inline ones included.
            if let Some(buffer) = io_buffer.as_deref_mut() {
                let idx = cursor.map_or(0, |c| c.index);
                for (call, (_id, tool_result)) in result.tool_calls.iter().zip(merged.iter()) {
                    buffer.logs.push((
                        idx,
                        format!("[tool] {}: {}", call.name, one_line(tool_result, 200)),
                    ));
                }
            }
            super::tool_results::apply_tool_results_with_parts(
                &mut window,
                super::tool_results::Reply {
                    text: &result.response,
                    parts: &result.parts,
                    stage: routing_stage,
                    sink: part_sink.as_ref(),
                },
                &result.tool_calls,
                &merged,
                routing.map(|c| &c.routing),
                sensitivities.map(|s| &s.0),
                result.reasoning.clone(),
            );
            commands
                .entity(entity)
                .remove::<ReadyForTools>()
                .remove::<(
                    RecoveredResults,
                    ResumedExecutions,
                    super::lane_batch::ResumedHold,
                )>()
                .insert(ReadyToInfer);
            continue;
        }
        // Lane work: decided call by call in the world, journaled and sent by
        // `dispatch_lane_batches`, which the batch is handed to here.
        commands
            .entity(entity)
            .remove::<(
                ReadyForTools,
                ResumedExecutions,
                super::lane_batch::ResumedHold,
            )>()
            .insert(
                super::lane_batch::PendingBatch::new(
                    lane_calls,
                    context_results,
                    executions,
                    recovered,
                    produced,
                )
                .resumed(resumed.is_some())
                .holding(hold),
            );
    }
}
