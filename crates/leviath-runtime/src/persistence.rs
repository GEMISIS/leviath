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
}

/// Running token + tool-call totals accumulated across an agent's inferences, for
/// the snapshot. Updated by the inference-collect system.
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, Eq)]
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
}

/// Run-scoped productivity flags, mirrored into `meta.json` so an empty run can
/// be recognized (and explained) from disk. Unlike [`StageProgress`], this is
/// never reset on a stage transition - it describes the whole run.
///
/// [`StageProgress`]: crate::pipeline::StageProgress
#[derive(Component, Clone, Default, Debug, PartialEq)]
pub struct RunOutcomeFlags(pub leviath_core::run_meta::RunFlags);

impl TokenTotals {
    /// Add one inference response's usage to the running totals.
    pub fn add_usage(&mut self, usage: &leviath_providers::TokenUsage) {
        self.prompt_tokens += usage.prompt_tokens;
        self.completion_tokens += usage.completion_tokens;
        self.cached_tokens += usage.cached_tokens;
        self.cache_write_tokens += usage.cache_write_tokens;
    }
}

/// Map an agent's ECS status to the on-disk [`RunStatus`].
pub fn run_status_from(status: &AgentStatus) -> RunStatus {
    match status {
        AgentStatus::Idle | AgentStatus::Active => RunStatus::Running,
        AgentStatus::Waiting => RunStatus::WaitingInput,
        AgentStatus::Complete => RunStatus::Complete,
        AgentStatus::Error { .. } => RunStatus::Error,
        AgentStatus::Cancelled => RunStatus::Cancelled,
    }
}

/// Map an agent's ECS status to the on-disk per-stage [`StageRunStatus`] for the
/// stage it is currently in. `Cancelled` has no stage-level equivalent, so it
/// surfaces as `Error` (the stage stopped without completing).
pub fn stage_status_from(status: &AgentStatus) -> StageRunStatus {
    match status {
        AgentStatus::Idle | AgentStatus::Active => StageRunStatus::Active,
        AgentStatus::Waiting => StageRunStatus::WaitingInput,
        AgentStatus::Complete => StageRunStatus::Complete,
        AgentStatus::Error { .. } | AgentStatus::Cancelled => StageRunStatus::Error,
    }
}

/// The stringified region kind used in snapshots (matches the dashboard reader).
fn region_kind_str(kind: &RegionKind) -> &'static str {
    match kind {
        RegionKind::Pinned => "pinned",
        RegionKind::Temporary => "temporary",
        RegionKind::Clearable => "clearable",
        RegionKind::SlidingWindow { .. } => "sliding",
        RegionKind::Compacting { .. } => "compacting",
        RegionKind::CompactHistory { .. } => "history",
        RegionKind::HashMap { .. } => "hashmap",
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
            current_tokens: r.current_tokens,
            max_tokens: r.max_tokens,
            entries: r
                .content
                .iter()
                .enumerate()
                .map(|(i, e)| RegionEntrySnapshot {
                    content: e.content_owned(),
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

/// Build the run metadata (`meta.json`) from an agent's live components, stamping
/// `updated_at` with `now_secs`. `stage_index` is the agent's current stage
/// position within its blueprint.
#[allow(clippy::too_many_arguments)]
pub fn build_run_meta(
    md: &RunMetadata,
    state: &AgentState,
    totals: &TokenTotals,
    flags: &RunOutcomeFlags,
    stage_index: usize,
    now_secs: i64,
    depth: usize,
    max_child_depth: usize,
) -> RunMeta {
    // `empty_output` is only meaningful once the run has stopped: a running
    // agent that hasn't written anything *yet* is not an empty run.
    let status = run_status_from(&state.status);
    let mut flags = flags.0.clone();
    flags.empty_output = matches!(
        status,
        RunStatus::Complete | RunStatus::Error | RunStatus::Cancelled
    ) && flags.modified_file_count == 0;
    RunMeta {
        run_id: md.run_id.clone(),
        agent_name: md.agent_name.clone(),
        agent_path: md.agent_path.clone(),
        task: md.task.clone(),
        model: md.model.clone(),
        pid: 0, // no per-run worker process in the shared world
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
        workdir: md.workdir.clone(),
        started_at: md.started_at,
        updated_at: now_secs,
        error: match &state.status {
            AgentStatus::Error { message } => Some(message.clone()),
            _ => None,
        },
        title: md.title.clone(),
        metadata: md.metadata.clone(),
        callback_url: md.callback_url.clone(),
        callback_secret: md.callback_secret.clone(),
        parent_run_id: md.parent_run_id.clone(),
        // The tree links, so restart can rebuild the exact parent→children graph.
        children: state.spawned_children_ids.clone(),
        depth,
        max_child_depth,
        flags,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::Region;
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
        }
    }

    #[test]
    fn status_mapping_covers_all_variants() {
        assert_eq!(run_status_from(&AgentStatus::Idle), RunStatus::Running);
        assert_eq!(run_status_from(&AgentStatus::Active), RunStatus::Running);
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
        });
        t.add_usage(&TokenUsage {
            prompt_tokens: 3,
            completion_tokens: 4,
            total_tokens: 7,
            cached_tokens: 0,
            cache_write_tokens: 0,
        });
        t.tool_calls = 6;
        assert_eq!(t.prompt_tokens, 13);
        assert_eq!(t.completion_tokens, 9);
        assert_eq!(t.cached_tokens, 2);
        assert_eq!(t.cache_write_tokens, 1);
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
        };
        let mut st = state(AgentStatus::Active);
        st.spawned_children_ids = vec!["child-a".to_string(), "child-b".to_string()];
        let meta = build_run_meta(
            &md,
            &st,
            &totals,
            &RunOutcomeFlags::default(),
            1,
            2000,
            1,
            4,
        );

        assert_eq!(meta.run_id, "run-1");
        assert_eq!(meta.status, RunStatus::Running);
        assert_eq!(meta.current_stage, "plan");
        assert_eq!(meta.stage_index, 1);
        assert_eq!(meta.iteration, 4);
        assert_eq!(meta.prompt_tokens, 100);
        assert_eq!(meta.tool_calls, 7);
        assert_eq!(meta.updated_at, 2000);
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
    }

    #[test]
    fn build_run_meta_flags_empty_output_only_once_the_run_has_stopped() {
        let mut flags = RunOutcomeFlags::default();
        flags.0.gates_forced = 2;
        // Still running with nothing written: not (yet) an empty run.
        let running = build_run_meta(
            &metadata(),
            &state(AgentStatus::Active),
            &TokenTotals::default(),
            &flags,
            0,
            1000,
            0,
            0,
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
                &metadata(),
                &state(status),
                &TokenTotals::default(),
                &flags,
                0,
                1000,
                0,
                0,
            );
            assert!(meta.flags.empty_output);
        }

        // Finished having written something: not empty.
        let mut wrote = RunOutcomeFlags::default();
        wrote.0.record_modification("src/a.rs");
        let meta = build_run_meta(
            &metadata(),
            &state(AgentStatus::Complete),
            &TokenTotals::default(),
            &wrote,
            0,
            1000,
            0,
            0,
        );
        assert!(!meta.flags.empty_output);
        assert_eq!(meta.flags.modified_files, vec!["src/a.rs".to_string()]);
    }

    #[test]
    fn build_run_meta_carries_error_message() {
        let meta = build_run_meta(
            &metadata(),
            &state(AgentStatus::Error {
                message: "boom".to_string(),
            }),
            &TokenTotals::default(),
            &RunOutcomeFlags::default(),
            2,
            3000,
            0,
            0,
        );
        assert_eq!(meta.status, RunStatus::Error);
        assert_eq!(meta.error.as_deref(), Some("boom"));
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
                "sliding",
                "compacting",
                "history",
                "hashmap",
                "custom"
            ]
        );
        // The pinned region's entry is captured.
        let pin = snap.regions.iter().find(|r| r.name == "pin").unwrap();
        assert_eq!(pin.entries.len(), 1);
        assert_eq!(pin.entries[0].content, "hello");
    }
}
