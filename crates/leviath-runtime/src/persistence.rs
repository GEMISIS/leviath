//! Agent-state persistence: turning a live ECS agent into the on-disk snapshot
//! the dashboard/API read (`meta.json` + `context.json` under the run directory).
//!
//! This module holds the **pure** serialization core - components that carry an
//! agent's run identity and running token totals, plus functions that build the
//! [`RunMeta`]/[`ContextSnapshot`] value types from an agent's live components.
//! It does no I/O; the async write lane and the snapshot-dispatch system layer on
//! top of these.

use bevy_ecs::prelude::*;
use leviath_core::RegionKind;
use leviath_core::run_meta::{
    ContextSnapshot, RegionEntrySnapshot, RegionSnapshot, RunMeta, RunStatus, StageRunStatus,
    WaitMarkers, wait_reason_from,
};

use crate::components::{AgentState, AgentStatus, ContextWindow};

/// Static per-agent run metadata (the parts of [`RunMeta`] that don't change as
/// the agent runs). Set once when the agent is spawned; the dynamic fields are
/// filled from the live components at snapshot time.
#[derive(Component, Clone)]
pub struct RunMetadata {
    /// The run's unique id (its directory name under the runs dir).
    pub run_id: String,
    /// The agent/blueprint name.
    pub agent_name: String,
    /// Absolute path to the agent manifest directory.
    pub agent_path: String,
    /// The task prompt.
    pub task: String,
    /// The resolved model label (provider/model), if known.
    pub model: Option<String>,
    /// Absolute working directory for tool execution.
    pub workdir: String,
    /// Total number of stages in the blueprint.
    pub num_stages: usize,
    /// When the run started (unix seconds).
    pub started_at: i64,
    /// Parent run id, for sub-agent runs.
    pub parent_run_id: Option<String>,
    /// Custom key-value metadata from the spawn request.
    pub metadata: std::collections::HashMap<String, String>,
    /// Webhook to POST on completion/error.
    pub callback_url: Option<String>,
    /// Optional shared secret for HMAC-SHA256 signing the webhook body.
    pub callback_secret: Option<String>,
    /// Short human-readable title (None until generated).
    pub title: Option<String>,
    /// Why [`Self::title`] is still `None`, once titling has given up.
    ///
    /// `None` means titling has not finished (or was never asked for), which is
    /// a different state from "it ran and could not produce a name" - and one
    /// nothing outside the daemon could tell apart before this, because the
    /// reason only ever reached a debug log.
    pub title_error: Option<String>,
    /// Whether this run is unattended (launched with `--yolo`).
    ///
    /// Recorded on the agent so anything holding the world can ask. Two things
    /// need it: the sub-agent and fan-out spawners, which pass it down so a
    /// child of an unattended run is unattended too, and `meta.json`, so a
    /// daemon restart resumes the run the way it was launched. Both used to
    /// hardcode "attended", which stranded unattended runs on prompts no one was
    /// there to answer.
    pub unattended: bool,
    /// How much of the blueprint's `[read_paths]` the config granted, resolved
    /// once at spawn (see [`ReadPathGrantCounts`]). `None` when the blueprint
    /// declares none, which is nearly every agent.
    ///
    /// [`ReadPathGrantCounts`]: leviath_core::run_meta::ReadPathGrantCounts
    pub read_paths: Option<leviath_core::run_meta::ReadPathGrantCounts>,
    /// The output shape the caller asked for at launch, if they overrode the
    /// blueprint's. Held so it reaches `meta.json` and survives a restart; the
    /// resolved per-stage shape lives on `StageInference`/`StageSetup`.
    pub output_request: Option<leviath_core::output::OutputSpec>,
    /// The `--model` the caller gave at launch, verbatim, if any. Held so it
    /// reaches `meta.json` and a restart replays the same override rather
    /// than pinning the run to whatever `model` resolved to.
    pub model_override: Option<String>,
}

/// Running token + tool-call totals accumulated across an agent's inferences, for
/// the snapshot. Updated by the inference-collect system.
// No `Eq`: the cost is an `f64`, which has no total equality. `PartialEq` is
// what the tests compare with.
#[derive(Component, Clone, Copy, Default, Debug, PartialEq)]
pub struct TokenTotals {
    /// Cumulative prompt tokens.
    pub prompt_tokens: usize,
    /// Cumulative completion tokens.
    pub completion_tokens: usize,
    /// Cumulative tokens read from provider cache.
    pub cached_tokens: usize,
    /// Cumulative tokens written to provider cache.
    pub cache_write_tokens: usize,
    /// Cumulative tool calls across all iterations.
    pub tool_calls: usize,
    /// What the run has spent, and how well that figure is known.
    ///
    /// Not a bare `f64`: a total that silently omits the calls it could not
    /// price looks authoritative and understates by however much it skipped.
    /// [`CostTotals`] keeps the priced part and the unpriced count apart so the
    /// difference stays visible.
    ///
    /// [`CostTotals`]: leviath_providers::CostTotals
    pub cost: leviath_providers::CostTotals,
}

/// Run-scoped productivity flags, mirrored into `meta.json` so an empty run can
/// be recognized (and explained) from disk. Unlike [`StageProgress`], this is
/// never reset on a stage transition - it describes the whole run.
///
/// [`StageProgress`]: crate::pipeline::StageProgress
#[derive(Component, Clone, Default, Debug, PartialEq)]
pub struct RunOutcomeFlags(pub leviath_core::run_meta::RunFlags);

impl RunOutcomeFlags {
    /// Seed a fresh run's flags from the blueprint it is about to run.
    ///
    /// Every counter starts at zero; the one thing decided here is
    /// [`no_output_tools`], which is fixed for the run's lifetime and so is
    /// answered once rather than re-derived on every persist tick.
    ///
    /// Judged across *every* stage, not only the ones the run reaches: a run
    /// cancelled in the first stage of an agent that writes files really did
    /// produce nothing, and should still say so.
    ///
    /// [`no_output_tools`]: leviath_core::run_meta::RunFlags::no_output_tools
    pub fn for_blueprint(bp: &leviath_core::Blueprint) -> Self {
        Self(leviath_core::run_meta::RunFlags {
            no_output_tools: !bp.stages.iter().any(stage_can_modify),
            ..Default::default()
        })
    }
}

/// The final output an agent has submitted, held on the agent entity until the
/// persistence lane copies it into `meta.json`.
///
/// Absent until `submit_output` is called, and replaced (not appended to) by a
/// later call: an agent that submits twice meant the second one. The stage name
/// travels inside so the enforcement gate can tell "this stage submitted" from
/// "an earlier one did".
#[derive(Component, Clone, Debug, PartialEq)]
pub struct FinalOutput(pub leviath_core::output::FinalOutput);

/// Whether `stage` advertises a tool whose writes the framework would record:
/// a built-in [`MODIFYING_TOOLS`] name, or one that this stage's own outgoing
/// transition gates name (the declared escape hatch for agents whose writes go
/// through MCP or script tools).
///
/// Deliberately the same test the transition gate applies in `gate_blocks`, so
/// a gated stage and the run's flags cannot disagree about what "can modify"
/// means.
/// `shell` is absent from both: an agent can edit through `sed -i` without the
/// framework seeing it, so shell capability is real but unverifiable - which
/// is exactly why such a run should still be reported as empty rather than
/// excused.
///
/// [`MODIFYING_TOOLS`]: leviath_core::blueprint::MODIFYING_TOOLS
fn stage_can_modify(stage: &leviath_core::Stage) -> bool {
    stage.available_tools.iter().any(|t| {
        let canonical = leviath_tools::canonical_tool_name(t);
        leviath_core::blueprint::MODIFYING_TOOLS.contains(&canonical)
            || stage
                .transitions
                .iter()
                .flat_map(|edges| edges.values())
                .filter_map(|edge| edge.gate.as_ref())
                .any(|gate| {
                    gate.tools
                        .iter()
                        .any(|extra| leviath_tools::canonical_tool_name(extra) == canonical)
                })
    })
}

impl TokenTotals {
    /// Add one inference response's usage to the running totals.
    pub fn add_usage(&mut self, usage: &leviath_providers::TokenUsage) {
        self.add_usage_priced(usage, None);
    }

    /// The same, also folding the call into the run's cost at `pricing`.
    ///
    /// `pricing` is the fallback: a call the provider priced itself is counted
    /// at that figure and these rates are never consulted.
    pub fn add_usage_priced(
        &mut self,
        usage: &leviath_providers::TokenUsage,
        pricing: Option<&leviath_providers::ModelPricing>,
    ) {
        self.prompt_tokens += usage.prompt_tokens;
        self.completion_tokens += usage.completion_tokens;
        self.cached_tokens += usage.cached_tokens;
        self.cache_write_tokens += usage.cache_write_tokens;
        self.cost.add(usage, pricing);
    }
}

/// Whether a run in `status` carrying `flags` stopped with nothing to show for
/// itself.
///
/// Four things have to hold. The run has to have *stopped* - an agent that
/// hasn't written anything yet is not an empty run, it is a busy one. It has to
/// have modified nothing. It must not have submitted a final output, which is
/// producing something even when no file changed. And its blueprint has to have
/// offered a way to modify something, or the question does not apply to it (see
/// [`no_output_tools`](leviath_core::run_meta::RunFlags::no_output_tools)).
///
/// One definition, called by both `meta.json` and the run listing, so what an
/// operator reads in `lev ps` and what a harness reads off disk cannot drift
/// apart.
pub fn is_empty_output(status: &AgentStatus, flags: &leviath_core::run_meta::RunFlags) -> bool {
    matches!(
        run_status_from(status),
        RunStatus::Complete | RunStatus::Error | RunStatus::Cancelled
    ) && flags.modified_file_count == 0
        && !flags.produced_output
        && !flags.no_output_tools
}

/// Map an agent's ECS status to the on-disk [`RunStatus`].
pub fn run_status_from(status: &AgentStatus) -> RunStatus {
    match status {
        AgentStatus::Idle | AgentStatus::Active => RunStatus::Running,
        AgentStatus::Paused => RunStatus::Paused,
        AgentStatus::Waiting => RunStatus::WaitingInput,
        AgentStatus::Complete => RunStatus::Complete,
        AgentStatus::Error { .. } => RunStatus::Error,
        AgentStatus::Cancelled => RunStatus::Cancelled,
    }
}

/// The [`RunStatus`] behind an engine status *label*, or `None` for a word this
/// build does not know.
///
/// [`run_status_from`] is the authority and takes the status itself; this is
/// the same mapping for a caller holding only the word. The gateway is that
/// caller: a [`WorldEvent`](crate::host::WorldEvent) crosses the control socket
/// with its status already flattened to a label, and the gateway has to name
/// the same state in the vocabulary its own clients read - `running` where the
/// engine says `idle` or `active`, `waiting_input` where it says `waiting`.
/// Doing that here rather than in the gateway is what keeps the mapping in one
/// place instead of copied into every console that watches the socket.
///
/// `None` means the daemon on the other end of the socket knows a status this
/// build does not, which is a version skew rather than a bad value: the caller
/// should pass the word through untranslated rather than invent a state.
pub fn run_status_for_label(label: &str) -> Option<RunStatus> {
    AgentStatus::from_label(label).map(|status| run_status_from(&status))
}

/// Map an agent's ECS status to the on-disk per-stage [`StageRunStatus`] for the
/// stage it is currently in. `Cancelled` has no stage-level equivalent, so it
/// surfaces as `Error` (the stage stopped without completing).
pub fn stage_status_from(status: &AgentStatus) -> StageRunStatus {
    match status {
        // A paused agent's current stage is still mid-flight, not a new stage state.
        AgentStatus::Idle | AgentStatus::Active | AgentStatus::Paused => StageRunStatus::Active,
        AgentStatus::Waiting => StageRunStatus::WaitingInput,
        AgentStatus::Complete => StageRunStatus::Complete,
        AgentStatus::Error { .. } | AgentStatus::Cancelled => StageRunStatus::Error,
    }
}

/// The stringified region kind used in snapshots and by the blueprint API.
///
/// One word per kind, and it is the word the blueprint's own TOML uses. It used
/// to be a third spelling of its own - `sliding` for a `sliding_window`,
/// `history` for a `compact_history` - which meant a console reading a context
/// snapshot and a console reading a blueprint disagreed about what the same
/// region was. Snapshots written by an older build still carry the old two
/// words, so a reader that renders kinds should accept both.
pub fn region_kind_str(kind: &RegionKind) -> &'static str {
    match kind {
        RegionKind::Pinned => "pinned",
        RegionKind::Temporary => "temporary",
        RegionKind::Clearable => "clearable",
        RegionKind::SlidingWindow { .. } => "sliding_window",
        RegionKind::Compacting { .. } => "compacting",
        RegionKind::CompactHistory { .. } => "compact_history",
        RegionKind::HashMap { .. } => "hashmap",
        RegionKind::Checklist => "checklist",
        RegionKind::Custom { .. } => "custom",
    }
}

/// Build the full context snapshot (`context.json`) from a window. Pure over the
/// window - no engine/entity. (Ported from the CLI's `build_context_snapshot`.)
pub fn build_context_snapshot(window: &ContextWindow, stage_name: &str) -> ContextSnapshot {
    let regions = window
        .regions
        .iter()
        .map(|r| RegionSnapshot {
            name: r.name.clone(),
            kind: region_kind_str(&r.kind).to_string(),
            description: r.description.clone(),
            current_tokens: r.current_tokens,
            max_tokens: r.max_tokens,
            entries: r
                .content
                .iter()
                .enumerate()
                .map(|(i, e)| RegionEntrySnapshot {
                    content: e.content.clone(),
                    tokens: e.tokens,
                    kind: e.kind.clone(),
                    metadata: e.metadata.clone(),
                    key: e.key.clone(),
                    // `None` when the region has no taint tracking (it is off,
                    // or this is an older region): `Public`, which is what a
                    // restore assumed anyway.
                    taint: r
                        .taint
                        .as_ref()
                        .and_then(|t| t.entry_taint(i))
                        .unwrap_or_default(),
                })
                .collect(),
        })
        .collect();
    ContextSnapshot {
        stage_name: stage_name.to_string(),
        total_tokens: window.current_tokens,
        max_tokens: window.max_tokens,
        regions,
    }
}

/// The agent components `meta.json` is built from.
///
/// Held apart from [`RunPosition`] because these are read off the entity while
/// the position is stamped onto it: one is what the agent *is*, the other is
/// where it has got to.
pub struct RunMetaSources<'a> {
    /// The run's immutable metadata, fixed at spawn.
    pub md: &'a RunMetadata,
    /// The agent's live state.
    pub state: &'a AgentState,
    /// Token totals accumulated so far.
    pub totals: &'a TokenTotals,
    /// Outcome flags the blueprint's shape decides.
    pub flags: &'a RunOutcomeFlags,
    /// The submitted answer, when the run has produced one.
    pub final_output: Option<&'a FinalOutput>,
    /// The parking markers the agent is carrying, read off the entity by the
    /// caller, which is where they are queryable.
    pub parked: WaitMarkers,
}

/// Where the run has got to, and when.
pub struct RunPosition {
    /// Index of the stage the agent is in.
    pub stage_index: usize,
    /// The moment `updated_at` is stamped with.
    pub now_secs: i64,
    /// When the run last actually moved, as distinct from last being touched.
    pub last_progress_at: Option<i64>,
    /// How deep in the sub-agent tree this run sits.
    pub depth: usize,
    /// How deep the tree may go.
    pub max_child_depth: usize,
}

/// Build the run metadata (`meta.json`) from an agent's live components, stamping
/// `updated_at` with `now_secs`. `stage_index` is the agent's current stage
/// position within its blueprint.
///
/// `last_progress_at` is the caller's separate record of when the run last
/// actually moved, which is not the same as `now_secs`: this is called on the
/// heartbeat too, and a heartbeat write must advance `updated_at` while leaving
/// the progress stamp where it was. Taken as a plain `Option` rather than the
/// watermark it comes from so this stays a data mapper with no dependency on the
/// persistence pipeline.
pub fn build_run_meta(sources: RunMetaSources<'_>, at: RunPosition) -> RunMeta {
    let RunMetaSources {
        md,
        state,
        totals,
        flags,
        final_output,
        parked,
    } = sources;
    let RunPosition {
        stage_index,
        now_secs,
        last_progress_at,
        depth,
        max_child_depth,
    } = at;
    let status = run_status_from(&state.status);
    let mut flags = flags.0.clone();
    // Having submitted an output is itself production, so this is settled before
    // the emptiness verdict rather than after it.
    flags.produced_output = final_output.is_some();
    flags.empty_output = is_empty_output(&state.status, &flags);
    RunMeta {
        run_id: md.run_id.clone(),
        agent_name: md.agent_name.clone(),
        agent_path: md.agent_path.clone(),
        task: md.task.clone(),
        model: md.model.clone(),
        pid: 0, // no per-run worker process in the shared world; see RunMeta::pid
        status,
        current_stage: state.current_stage.clone(),
        stage_index,
        num_stages: md.num_stages,
        iteration: state.iteration,
        prompt_tokens: totals.prompt_tokens,
        completion_tokens: totals.completion_tokens,
        cached_tokens: totals.cached_tokens,
        cache_write_tokens: totals.cache_write_tokens,
        tool_calls: totals.tool_calls,
        // `None` when any call went unpriced: an understated total that looks
        // authoritative is worse than an honest absence.
        cost_usd: totals.cost.total_usd(),
        unpriced_calls: totals.cost.unpriced_calls,
        cost_is_exact: totals.cost.is_exact(),
        cost_priced_usd: totals.cost.priced_usd,
        workdir: md.workdir.clone(),
        started_at: md.started_at,
        updated_at: now_secs,
        last_progress_at,
        error: match &state.status {
            AgentStatus::Error { message } => Some(message.clone()),
            _ => None,
        },
        title: md.title.clone(),
        title_error: md.title_error.clone(),
        metadata: md.metadata.clone(),
        callback_url: md.callback_url.clone(),
        callback_secret: md.callback_secret.clone(),
        parent_run_id: md.parent_run_id.clone(),
        // The tree links, so restart can rebuild the exact parent→children graph.
        children: state.spawned_children_ids.clone(),
        depth,
        max_child_depth,
        flags,
        yolo: md.unattended,
        read_paths: md.read_paths,
        final_output: final_output.map(|o| o.0.descriptor()),
        // Paused counts as parked here, not just Waiting: a run held until the
        // machine is fixed is exactly the case where a reader most needs to be
        // told why, and it is `Paused` rather than `Waiting` because nothing is
        // holding a prompt open for it.
        waiting_on: wait_reason_from(
            matches!(state.status, AgentStatus::Waiting | AgentStatus::Paused),
            &parked,
        ),
        output_request: md.output_request.clone(),
        model_override: md.model_override.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::Region;
    use leviath_core::run_meta::WaitReason;
    use leviath_providers::TokenUsage;

    fn state(status: AgentStatus) -> AgentState {
        AgentState {
            agent_id: "a".to_string(),
            current_stage: "plan".to_string(),
            iteration: 4,
            status,
            spawned_children_ids: vec![],
            pending_wait: None,
            accepts_messages: true,
        }
    }

    fn metadata() -> RunMetadata {
        RunMetadata {
            run_id: "run-1".to_string(),
            agent_name: "coder".to_string(),
            agent_path: "/agents/coder".to_string(),
            task: "do it".to_string(),
            model: Some("anthropic/claude".to_string()),
            workdir: "/work".to_string(),
            num_stages: 3,
            started_at: 1000,
            parent_run_id: Some("parent".to_string()),
            metadata: std::collections::HashMap::from([("k".to_string(), "v".to_string())]),
            callback_url: Some("http://cb".to_string()),
            callback_secret: Some("sekret".to_string()),
            title: Some("Do It".to_string()),
            title_error: None,
            unattended: false,
            read_paths: None,
            output_request: None,
            model_override: None,
        }
    }

    /// A stage advertising `tools`, with `gate_tools` named by the gate on its
    /// single outgoing edge. `gate_tools: None` gives the stage no transitions
    /// at all, which is the other half of the `Option` the scan walks.
    fn stage_with(tools: &[&str], gate_tools: Option<&[&str]>) -> leviath_core::Stage {
        let mut stage = leviath_core::Stage::new(
            "s".to_string(),
            leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
        );
        stage.available_tools = tools.iter().map(|t| (*t).to_string()).collect();
        stage.transitions = gate_tools.map(|extra| {
            let gate = (!extra.is_empty()).then(|| leviath_core::blueprint::TransitionGate {
                require_modifications: true,
                tools: extra.iter().map(|t| (*t).to_string()).collect(),
                ..Default::default()
            });
            std::collections::HashMap::from([(
                "next".to_string(),
                leviath_core::blueprint::TransitionEdge {
                    target: "next".to_string(),
                    condition: leviath_core::blueprint::TransitionCondition::Always,
                    hint: None,
                    transform: leviath_core::blueprint::EdgeTransform::Direct,
                    gate,
                    stuck: None,
                },
            )])
        });
        stage
    }

    fn blueprint_of(stages: Vec<leviath_core::Stage>) -> leviath_core::Blueprint {
        leviath_core::Blueprint::new(
            "bp".to_string(),
            "d".to_string(),
            stages,
            leviath_core::ContextLayout::new(vec![], 1000),
        )
    }

    fn no_output_tools(stages: Vec<leviath_core::Stage>) -> bool {
        RunOutcomeFlags::for_blueprint(&blueprint_of(stages))
            .0
            .no_output_tools
    }

    #[test]
    fn for_blueprint_asks_whether_any_stage_could_have_written() {
        // A blueprint with no stages at all offers nothing.
        assert!(no_output_tools(vec![]));
        // Read-only, and the sub-agent tools a router would use: nothing the
        // framework tracks as a file change. This is the issue #192 case.
        assert!(no_output_tools(vec![stage_with(
            &["read_file", "spawn_agent", "context_write"],
            None
        )]));
        // `shell` confers no tracked write: an agent editing through `sed -i`
        // leaves no record, so silence from it stays suspicious rather than
        // excused. The alias resolves, so `bash` is judged as `shell`.
        assert!(no_output_tools(vec![stage_with(&["bash"], None)]));
        // A built-in modifying tool, under either name.
        assert!(!no_output_tools(vec![stage_with(&["write_file"], None)]));
        assert!(!no_output_tools(vec![stage_with(&["edit_file"], None)]));
        // Only one stage needs it.
        assert!(!no_output_tools(vec![
            stage_with(&["read_file"], None),
            stage_with(&["write_file"], None),
        ]));
    }

    #[test]
    fn for_blueprint_honors_a_gate_declaring_its_own_write_tool() {
        // An MCP/script write tool the stage advertises AND a gate names is a
        // tracked write - the same escape hatch `stage_modifying_tools` gives.
        assert!(!no_output_tools(vec![stage_with(
            &["mcp__fs__put"],
            Some(&["mcp__fs__put"])
        )]));
        // Declared by the gate but never advertised: the stage cannot call it.
        assert!(no_output_tools(vec![stage_with(
            &["read_file"],
            Some(&["mcp__fs__put"])
        )]));
        // Transitions present, but no gate on the edge.
        assert!(no_output_tools(vec![stage_with(&["read_file"], Some(&[]))]));
        // A gate that names a tool unrelated to what the stage advertises.
        assert!(no_output_tools(vec![stage_with(
            &["mcp__fs__put"],
            Some(&["mcp__other__put"])
        )]));
    }

    #[test]
    fn is_empty_output_needs_a_stopped_run_that_could_have_written() {
        let nothing = leviath_core::run_meta::RunFlags::default();
        // Running: it has not finished not-writing yet.
        assert!(!is_empty_output(&AgentStatus::Active, &nothing));
        assert!(!is_empty_output(&AgentStatus::Idle, &nothing));
        assert!(!is_empty_output(&AgentStatus::Paused, &nothing));
        assert!(!is_empty_output(&AgentStatus::Waiting, &nothing));
        // Every way of stopping counts.
        for status in [
            AgentStatus::Complete,
            AgentStatus::Cancelled,
            AgentStatus::Error {
                message: "x".to_string(),
            },
        ] {
            assert!(is_empty_output(&status, &nothing));
        }
        // Wrote something.
        let mut wrote = leviath_core::run_meta::RunFlags::default();
        wrote.record_modification("src/a.rs");
        assert!(!is_empty_output(&AgentStatus::Complete, &wrote));
        // Had nothing to write with.
        let incapable = leviath_core::run_meta::RunFlags {
            no_output_tools: true,
            ..Default::default()
        };
        assert!(!is_empty_output(&AgentStatus::Complete, &incapable));
    }

    #[test]
    fn status_mapping_covers_all_variants() {
        assert_eq!(run_status_from(&AgentStatus::Idle), RunStatus::Running);
        assert_eq!(run_status_from(&AgentStatus::Active), RunStatus::Running);
        assert_eq!(run_status_from(&AgentStatus::Paused), RunStatus::Paused);
        assert_eq!(
            run_status_from(&AgentStatus::Waiting),
            RunStatus::WaitingInput
        );
        assert_eq!(run_status_from(&AgentStatus::Complete), RunStatus::Complete);
        assert_eq!(
            run_status_from(&AgentStatus::Error {
                message: "x".to_string()
            }),
            RunStatus::Error
        );
        assert_eq!(
            run_status_from(&AgentStatus::Cancelled),
            RunStatus::Cancelled
        );
    }

    /// Going through the label must land where going through the status lands.
    /// The gateway only ever has the label, so if these two disagree the
    /// websocket and the REST routes describe the same run differently - which
    /// is the whole bug this mapping exists to close.
    #[test]
    fn the_label_route_and_the_status_route_agree() {
        for status in [
            AgentStatus::Idle,
            AgentStatus::Active,
            AgentStatus::Waiting,
            AgentStatus::Paused,
            AgentStatus::Complete,
            AgentStatus::Error {
                message: "x".to_string(),
            },
            AgentStatus::Cancelled,
        ] {
            assert_eq!(
                run_status_for_label(status.label()),
                Some(run_status_from(&status))
            );
        }
    }

    /// A word this build does not know is a daemon newer than the reader, not a
    /// state to invent: the caller gets `None` and passes the word through.
    #[test]
    fn an_unknown_label_maps_to_nothing() {
        assert_eq!(run_status_for_label("hibernating"), None);
        assert_eq!(run_status_for_label(""), None);
    }

    #[test]
    fn stage_status_mapping_covers_all_variants() {
        use leviath_core::run_meta::StageRunStatus;
        assert_eq!(
            stage_status_from(&AgentStatus::Idle),
            StageRunStatus::Active
        );
        assert_eq!(
            stage_status_from(&AgentStatus::Active),
            StageRunStatus::Active
        );
        assert_eq!(
            stage_status_from(&AgentStatus::Paused),
            StageRunStatus::Active
        );
        assert_eq!(
            stage_status_from(&AgentStatus::Waiting),
            StageRunStatus::WaitingInput
        );
        assert_eq!(
            stage_status_from(&AgentStatus::Complete),
            StageRunStatus::Complete
        );
        assert_eq!(
            stage_status_from(&AgentStatus::Error {
                message: "x".to_string()
            }),
            StageRunStatus::Error
        );
        assert_eq!(
            stage_status_from(&AgentStatus::Cancelled),
            StageRunStatus::Error
        );
    }

    #[test]
    fn token_totals_accumulate() {
        let mut t = TokenTotals::default();
        t.add_usage(&TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cached_tokens: 2,
            cache_write_tokens: 1,
            reported_cost_usd: None,
        });
        t.add_usage(&TokenUsage {
            prompt_tokens: 3,
            completion_tokens: 4,
            total_tokens: 7,
            cached_tokens: 0,
            cache_write_tokens: 0,
            reported_cost_usd: None,
        });
        t.tool_calls = 6;
        assert_eq!(t.prompt_tokens, 13);
        assert_eq!(t.completion_tokens, 9);
        assert_eq!(t.cached_tokens, 2);
        assert_eq!(t.cache_write_tokens, 1);
    }

    /// Each marker names its own reason, and only while the run is parked.
    ///
    /// The precedence itself is `leviath_core`'s, shared with the live
    /// listing; this pins that the persistence path feeds it the right
    /// markers.
    #[test]
    fn each_parking_marker_names_its_own_reason() {
        let cases = [
            (
                WaitMarkers {
                    gate_prompt: true,
                    ..Default::default()
                },
                WaitReason::TaintGate,
            ),
            (
                WaitMarkers {
                    interaction_point: true,
                    ..Default::default()
                },
                WaitReason::InteractionPoint,
            ),
            (
                WaitMarkers {
                    fan_out_outstanding: Some(3),
                    ..Default::default()
                },
                WaitReason::FanOutWorkers { outstanding: 3 },
            ),
            (
                WaitMarkers {
                    children_outstanding: Some(2),
                    ..Default::default()
                },
                WaitReason::Children { outstanding: 2 },
            ),
        ];
        for (markers, expected) in cases {
            assert_eq!(
                wait_reason_from(true, &markers),
                Some(expected.clone()),
                "{markers:?}"
            );
            // The same markers on a run that is not parked say nothing: an
            // active or finished run is not waiting on anybody.
            assert_eq!(wait_reason_from(false, &markers), None, "{markers:?}");
        }
    }

    /// The whole point of the field: a fan-out parent is not waiting on a
    /// person, and must not be reported as if it were.
    #[test]
    fn a_fan_out_parent_is_never_reported_as_waiting_on_a_person() {
        let reason = wait_reason_from(
            true,
            &WaitMarkers {
                fan_out_outstanding: Some(8),
                ..Default::default()
            },
        )
        .expect("a parked parent has a reason");
        assert_eq!(reason, WaitReason::FanOutWorkers { outstanding: 8 });
        assert!(
            !reason.needs_a_person(),
            "its workers are still going; nobody is needed"
        );
    }

    /// The reason reaches `meta.json`, which is the file every client reads.
    #[test]
    fn build_run_meta_records_why_a_run_is_parked() {
        let meta = build_run_meta(
            RunMetaSources {
                md: &metadata(),
                state: &state(AgentStatus::Waiting),
                totals: &TokenTotals::default(),
                flags: &RunOutcomeFlags::default(),
                final_output: None,
                parked: WaitMarkers {
                    children_outstanding: Some(2),
                    ..Default::default()
                },
            },
            RunPosition {
                stage_index: 0,
                now_secs: 0,
                last_progress_at: None,
                depth: 0,
                max_child_depth: 0,
            },
        );
        assert_eq!(meta.status, RunStatus::WaitingInput);
        assert_eq!(
            meta.waiting_on,
            Some(WaitReason::Children { outstanding: 2 })
        );
    }

    #[test]
    fn build_run_meta_fills_dynamic_and_static_fields() {
        let md = metadata();
        let totals = TokenTotals {
            prompt_tokens: 100,
            completion_tokens: 50,
            cached_tokens: 10,
            cache_write_tokens: 5,
            tool_calls: 7,
            cost: Default::default(),
        };
        let mut st = state(AgentStatus::Active);
        st.spawned_children_ids = vec!["child-a".to_string(), "child-b".to_string()];
        let meta = build_run_meta(
            RunMetaSources {
                md: &md,
                state: &st,
                totals: &totals,
                flags: &RunOutcomeFlags::default(),
                final_output: None,
                parked: WaitMarkers::default(),
            },
            RunPosition {
                stage_index: 1,
                now_secs: 2000,
                last_progress_at: Some(1900),
                depth: 1,
                max_child_depth: 4,
            },
        );

        assert_eq!(meta.run_id, "run-1");
        assert_eq!(meta.status, RunStatus::Running);
        assert_eq!(meta.current_stage, "plan");
        assert_eq!(meta.stage_index, 1);
        assert_eq!(meta.iteration, 4);
        assert_eq!(meta.prompt_tokens, 100);
        assert_eq!(meta.tool_calls, 7);
        assert_eq!(meta.updated_at, 2000);
        // The two stamps are independent: this snapshot was written at 2000, and
        // the run last moved at 1900. A heartbeat write is exactly that shape.
        assert_eq!(meta.last_progress_at, Some(1900));
        assert_eq!(meta.parent_run_id.as_deref(), Some("parent"));
        assert_eq!(meta.callback_url.as_deref(), Some("http://cb"));
        assert_eq!(meta.callback_secret.as_deref(), Some("sekret"));
        assert!(meta.error.is_none());
        // The tree links are carried through from the agent's live state.
        assert_eq!(
            meta.children,
            vec!["child-a".to_string(), "child-b".to_string()]
        );
        assert_eq!(meta.depth, 1);
        assert_eq!(meta.max_child_depth, 4);
        // Attended by default, so an ordinary run is never written as unattended.
        assert!(!meta.yolo);
    }

    /// The snapshot carries `unattended` through to `meta.json`, which is what a
    /// daemon restart reads back to resume the run the way it was launched.
    #[test]
    fn build_run_meta_records_an_unattended_run() {
        let mut md = metadata();
        md.unattended = true;
        let meta = build_run_meta(
            RunMetaSources {
                md: &md,
                state: &state(AgentStatus::Active),
                totals: &TokenTotals::default(),
                flags: &RunOutcomeFlags::default(),
                final_output: None,
                parked: WaitMarkers::default(),
            },
            RunPosition {
                stage_index: 1,
                now_secs: 2000,
                last_progress_at: None,
                depth: 1,
                max_child_depth: 4,
            },
        );
        assert!(meta.yolo);
    }

    #[test]
    fn build_run_meta_flags_empty_output_only_once_the_run_has_stopped() {
        let mut flags = RunOutcomeFlags::default();
        flags.0.gates_forced = 2;
        // Still running with nothing written: not (yet) an empty run.
        let running = build_run_meta(
            RunMetaSources {
                md: &metadata(),
                state: &state(AgentStatus::Active),
                totals: &TokenTotals::default(),
                flags: &flags,
                final_output: None,
                parked: WaitMarkers::default(),
            },
            RunPosition {
                stage_index: 0,
                now_secs: 1000,
                last_progress_at: None,
                depth: 0,
                max_child_depth: 0,
            },
        );
        assert!(!running.flags.empty_output);
        assert_eq!(running.flags.gates_forced, 2);

        // Finished with nothing written: that is the #107 signature.
        for status in [
            AgentStatus::Complete,
            AgentStatus::Cancelled,
            AgentStatus::Error {
                message: "x".to_string(),
            },
        ] {
            let meta = build_run_meta(
                RunMetaSources {
                    md: &metadata(),
                    state: &state(status),
                    totals: &TokenTotals::default(),
                    flags: &flags,
                    final_output: None,
                    parked: WaitMarkers::default(),
                },
                RunPosition {
                    stage_index: 0,
                    now_secs: 1000,
                    last_progress_at: None,
                    depth: 0,
                    max_child_depth: 0,
                },
            );
            assert!(meta.flags.empty_output);
        }

        // Finished having written something: not empty.
        let mut wrote = RunOutcomeFlags::default();
        wrote.0.record_modification("src/a.rs");
        let meta = build_run_meta(
            RunMetaSources {
                md: &metadata(),
                state: &state(AgentStatus::Complete),
                totals: &TokenTotals::default(),
                flags: &wrote,
                final_output: None,
                parked: WaitMarkers::default(),
            },
            RunPosition {
                stage_index: 0,
                now_secs: 1000,
                last_progress_at: None,
                depth: 0,
                max_child_depth: 0,
            },
        );
        assert!(!meta.flags.empty_output);
        assert_eq!(meta.flags.modified_files, vec!["src/a.rs".to_string()]);

        // Finished having written nothing, with nothing to write *with*: the
        // framework has no basis to call this empty, so it doesn't (issue #192).
        let mut incapable = RunOutcomeFlags::default();
        incapable.0.no_output_tools = true;
        let meta = build_run_meta(
            RunMetaSources {
                md: &metadata(),
                state: &state(AgentStatus::Complete),
                totals: &TokenTotals::default(),
                flags: &incapable,
                final_output: None,
                parked: WaitMarkers::default(),
            },
            RunPosition {
                stage_index: 0,
                now_secs: 1000,
                last_progress_at: None,
                depth: 0,
                max_child_depth: 0,
            },
        );
        assert!(!meta.flags.empty_output);
        assert!(meta.flags.no_output_tools);
    }

    #[test]
    fn build_run_meta_carries_error_message() {
        let meta = build_run_meta(
            RunMetaSources {
                md: &metadata(),
                state: &state(AgentStatus::Error {
                    message: "boom".to_string(),
                }),
                totals: &TokenTotals::default(),
                flags: &RunOutcomeFlags::default(),
                final_output: None,
                parked: WaitMarkers::default(),
            },
            RunPosition {
                stage_index: 2,
                now_secs: 3000,
                last_progress_at: None,
                depth: 0,
                max_child_depth: 0,
            },
        );
        assert_eq!(meta.status, RunStatus::Error);
        assert_eq!(meta.error.as_deref(), Some("boom"));
    }

    /// A submitted output reaches `meta.json` and settles the emptiness verdict.
    ///
    /// The second half is the point: an agent whose whole deliverable is its
    /// answer modifies no files, and before `produced_output` existed every one
    /// of its successful runs was reported `complete (no output)`.
    #[test]
    fn build_run_meta_carries_a_submitted_output_and_clears_the_empty_verdict() {
        let submitted = FinalOutput(leviath_core::output::FinalOutput::new(
            "Renamed two helpers and updated their callers.",
            Some("markdown".to_string()),
            "summary".to_string(),
            1234,
        ));
        let meta = build_run_meta(
            RunMetaSources {
                md: &metadata(),
                state: &state(AgentStatus::Complete),
                totals: &TokenTotals::default(),
                flags: &RunOutcomeFlags::default(),
                final_output: Some(&submitted),
                parked: WaitMarkers::default(),
            },
            RunPosition {
                stage_index: 0,
                now_secs: 1000,
                last_progress_at: None,
                depth: 0,
                max_child_depth: 0,
            },
        );
        let carried = meta.final_output.expect("the submission reached meta.json");
        // The descriptor, not the bytes: `meta.json` is parsed for every run on
        // every listing, so the answer itself lives in a sidecar beside it.
        assert_eq!(
            carried.bytes,
            "Renamed two helpers and updated their callers.".len()
        );
        assert_eq!(carried.format.as_deref(), Some("markdown"));
        assert_eq!(carried.stage, "summary");
        assert!(meta.flags.produced_output);
        // Modified nothing, yet produced something: not an empty run.
        assert!(!meta.flags.empty_output);
    }

    /// The same run without the submission is still judged empty, so the clause
    /// above is doing the work rather than some other condition.
    #[test]
    fn a_run_that_submits_nothing_is_still_judged_empty() {
        let meta = build_run_meta(
            RunMetaSources {
                md: &metadata(),
                state: &state(AgentStatus::Complete),
                totals: &TokenTotals::default(),
                flags: &RunOutcomeFlags::default(),
                final_output: None,
                parked: WaitMarkers::default(),
            },
            RunPosition {
                stage_index: 0,
                now_secs: 1000,
                last_progress_at: None,
                depth: 0,
                max_child_depth: 0,
            },
        );
        assert!(meta.final_output.is_none());
        assert!(!meta.flags.produced_output);
        assert!(meta.flags.empty_output);
    }

    #[test]
    fn context_snapshot_captures_all_region_kinds() {
        let mut w = ContextWindow::new(1000);
        w.add_region(Region::new("pin".to_string(), RegionKind::Pinned, 100));
        w.add_region(Region::new("tmp".to_string(), RegionKind::Temporary, 100));
        w.add_region(Region::new("clr".to_string(), RegionKind::Clearable, 100));
        w.add_region(Region::new(
            "slide".to_string(),
            RegionKind::SlidingWindow {
                max_items: 5,
                eviction_strategy: leviath_core::EvictionStrategy::PerItem,
            },
            100,
        ));
        w.add_region(Region::new(
            "comp".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 5,
            },
            100,
        ));
        w.add_region(Region::new(
            "hist".to_string(),
            RegionKind::CompactHistory {
                source_region: "comp".to_string(),
            },
            100,
        ));
        w.add_region(Region::new(
            "map".to_string(),
            RegionKind::HashMap { max_entries: None },
            100,
        ));
        w.add_region(Region::new(
            "brain".to_string(),
            RegionKind::Custom {
                script: "b.rhai".to_string(),
                persistent: false,
            },
            100,
        ));
        w.add_region(Region::new("todos".to_string(), RegionKind::Checklist, 100));
        // Carried onto the snapshot so every reader of context.json can explain
        // a region it is already displaying, rather than each one having to
        // find and re-parse the manifest.
        let mut described = Region::new("sources".to_string(), RegionKind::Pinned, 100);
        described.description = Some("One line per source.".to_string());
        w.add_region(described);
        let _ = w.add_to_region("pin", "hello".to_string(), 3);
        w.current_tokens = w.calculate_tokens();

        let snap = build_context_snapshot(&w, "plan");

        assert_eq!(snap.stage_name, "plan");
        let kinds: Vec<&str> = snap.regions.iter().map(|r| r.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "pinned",
                "temporary",
                "clearable",
                // The blueprint's own words for these two, not a spelling that
                // only ever appeared in a snapshot.
                "sliding_window",
                "compacting",
                "compact_history",
                "hashmap",
                "custom",
                "checklist",
                "pinned"
            ]
        );
        let described = snap.regions.iter().find(|r| r.name == "sources").unwrap();
        assert_eq!(
            described.description.as_deref(),
            Some("One line per source.")
        );
        let pin_desc = snap.regions.iter().find(|r| r.name == "pin").unwrap();
        assert_eq!(pin_desc.description, None);
        // The pinned region's entry is captured.
        let pin = snap.regions.iter().find(|r| r.name == "pin").unwrap();
        assert_eq!(pin.entries.len(), 1);
        assert_eq!(pin.entries[0].content, "hello");
    }
}
