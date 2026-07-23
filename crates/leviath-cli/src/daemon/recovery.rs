//! Restart recovery: reload persisted non-terminal agents into a fresh world when
//! the daemon starts, so runs interrupted by a stop/crash resume where they left
//! off — critically, any agent that was mid-inference re-issues that inference
//! (the reloaded agent is `ReadyToInfer`), rather than being lost.
//!
//! For each `<runs_dir>/<run_id>/meta.json` whose status is non-terminal, this
//! loads the blueprint (via [`build_agent_for_reload`], reusing the spawn path),
//! which skips the required-at-spawn region gate since the window is restored
//! from a snapshot; restores the
//! persisted context / stage / iteration / token totals via
//! [`leviath_runtime::restore::restore_agent`], and preserves the original run
//! metadata. Anything unreadable or un-reloadable is skipped (logged), never fatal.
//!
//! One exception to the "re-issue inference" resume: a run that was parked at a
//! stage-boundary interaction point (e.g. `plan_approval`) wrote an
//! `interactions.json` sidecar while blocked. For those, `reload_one` calls
//! [`leviath_runtime::interaction_points::restore_interaction_point`] to bring the
//! agent back in the *waiting* state with the same prompt re-opened, rather than
//! re-inferring and dropping it (issue #38). Model-initiated dynamic tools
//! (`ask_user_*`, `present_for_review`, `edit_document`) and taint-gate prompts are
//! not persisted — they block inside the transient tool-worker turn, so on restart
//! they take the ordinary re-inference path and the model simply re-asks.

use std::path::Path;
use std::sync::Arc;

use bevy_ecs::entity::Entity;
use leviath_core::run_meta::{ContextSnapshot, RunMeta, RunStatus};
use leviath_mcp::ToolExecutor;
use leviath_providers::Tool;
use leviath_runtime::host::{SpawnArgs, SubAgentOp};
use leviath_runtime::interaction_hub::InteractionHub;
use leviath_runtime::interaction_points::InteractionPointState;
use leviath_runtime::persistence::{RunMetadata, TokenTotals};
use leviath_runtime::restore::restore_agent;
use leviath_runtime::world::PipelineWorld;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::Config;
use crate::daemon::spawn::build_agent_for_reload;
use crate::daemon::tool_service::CliToolService;

/// Reload every non-terminal persisted run under `runs_dir`, returning the
/// `(run_id, entity)` pairs for the host to map. Runs that fail to reload are
/// skipped.
#[allow(clippy::too_many_arguments)]
pub fn reload_persisted_agents(
    world: &mut PipelineWorld,
    tool_service: &CliToolService,
    config: &Config,
    shared_mcp: Arc<Mutex<ToolExecutor>>,
    mcp_tool_defs: &[Tool],
    hub: &InteractionHub,
    runs_dir: &Path,
    now_secs: i64,
    subagent_tx: &UnboundedSender<SubAgentOp>,
) -> Vec<(String, Entity)> {
    let mut reloaded: Vec<(RunMeta, Entity)> = Vec::new();
    let Ok(dir_entries) = std::fs::read_dir(runs_dir) else {
        return Vec::new(); // no runs dir yet — nothing to recover
    };
    // Scan phase: collect every persisted run's metadata + whether it's parked mid
    // fan-out (has a fanout.json), so the triage can rank them.
    let candidates: Vec<(RunMeta, bool)> = dir_entries
        .flatten()
        .filter_map(|dir_entry| {
            let run_dir = dir_entry.path();
            let meta = read_meta(&run_dir)?; // no meta.json, or unreadable/unparseable
            let parked_on_fanout = run_dir.join("fanout.json").exists();
            Some((meta, parked_on_fanout))
        })
        .collect();
    // Order phase: drop terminal runs and rank the rest actionable-first (in-flight
    // inference / pending tool results before blocked-on-input), so interrupted work
    // that can make progress resumes ahead of runs that can't.
    let ordered = leviath_runtime::restore::triage_restores(candidates);
    for meta in ordered {
        let run_dir = runs_dir.join(&meta.run_id);
        match reload_one(
            world,
            tool_service,
            config,
            shared_mcp.clone(),
            mcp_tool_defs,
            hub,
            &meta,
            &run_dir,
            now_secs,
            subagent_tx,
        ) {
            Ok(entity) => reloaded.push((meta, entity)),
            Err(e) => {
                tracing::warn!(run_id = %meta.run_id, error = %e, "skipping un-reloadable run");
            }
        }
    }
    // Second pass: every run is now an entity, so rebuild the parent→children
    // tree deterministically from the persisted links (no heuristics), then
    // resume any parent that was parked mid fan-out.
    relink_tree(world, &reloaded);
    restore_fan_outs(world, &reloaded, runs_dir);
    reloaded
        .into_iter()
        .map(|(meta, entity)| (meta.run_id, entity))
        .collect()
}

/// Page a single unloaded run back into the world from disk, on demand. Reads
/// its persisted metadata; if the run exists and is non-terminal, reloads it
/// (blueprint + tool state + context/stage) and returns the new entity. `None`
/// if there's no such resumable run. This is the host's reload-on-demand seam
/// (an op targeting an unloaded run pages it in first).
#[allow(clippy::too_many_arguments)]
pub fn reload_run(
    world: &mut PipelineWorld,
    tool_service: &CliToolService,
    config: &Config,
    shared_mcp: Arc<Mutex<ToolExecutor>>,
    mcp_tool_defs: &[Tool],
    hub: &InteractionHub,
    run_id: &str,
    runs_dir: &std::path::Path,
    now_secs: i64,
    subagent_tx: &UnboundedSender<SubAgentOp>,
) -> Option<Entity> {
    let run_dir = runs_dir.join(run_id);
    let meta = read_meta(&run_dir)?;
    if is_terminal(&meta.status) {
        return None; // a finished run isn't paged back in
    }
    reload_one(
        world,
        tool_service,
        config,
        shared_mcp,
        mcp_tool_defs,
        hub,
        &meta,
        &run_dir,
        now_secs,
        subagent_tx,
    )
    .ok()
}

/// Rebuild `FanOutWaiting` for any reloaded parent that was parked mid fan-out
/// (a `<run_dir>/fanout.json` is present), so its split/merge resumes rather than
/// hanging. Active workers are re-linked by run-id via the reloaded run→entity
/// map; a worker that didn't reload is recorded as a failure so the merge still
/// completes. A malformed/absent file is skipped.
fn restore_fan_outs(world: &mut PipelineWorld, reloaded: &[(RunMeta, Entity)], runs_dir: &Path) {
    let by_run_id: std::collections::HashMap<&str, Entity> = reloaded
        .iter()
        .map(|(m, e)| (m.run_id.as_str(), *e))
        .collect();
    for (meta, entity) in reloaded {
        let path = runs_dir.join(&meta.run_id).join("fanout.json");
        let Some(state) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<leviath_runtime::fanout::FanOutState>(&s).ok())
        else {
            continue;
        };
        leviath_runtime::fanout::restore_fan_out_waiting(
            world.world_mut(),
            *entity,
            state,
            &|rid| by_run_id.get(rid).copied(),
        );
    }
}

/// Rebuild `ParentRef` / `SubAgentChildren` on the freshly reloaded entities from
/// their persisted `parent_run_id` / `children` links, so a restarted daemon
/// resumes the exact sub-agent tree (a waiting parent holds for its children;
/// children aren't orphaned). Links whose counterpart didn't reload are logged
/// and skipped. Idempotent: existing components are overwritten, not duplicated.
fn relink_tree(world: &mut PipelineWorld, reloaded: &[(RunMeta, Entity)]) {
    use leviath_runtime::components::{AgentState, ParentRef, SubAgentChildren};

    let by_run_id: std::collections::HashMap<&str, Entity> = reloaded
        .iter()
        .map(|(m, e)| (m.run_id.as_str(), *e))
        .collect();
    let w = world.world_mut();
    for (meta, entity) in reloaded {
        // Child → parent edge.
        if let Some(parent_id) = &meta.parent_run_id {
            match by_run_id.get(parent_id.as_str()) {
                Some(&parent_entity) => {
                    w.entity_mut(*entity).insert(ParentRef {
                        parent_entity,
                        parent_agent_id: parent_id.clone(),
                        depth: meta.depth,
                    });
                }
                None => tracing::warn!(
                    run_id = %meta.run_id, parent = %parent_id,
                    "parent run did not reload; leaving child unlinked"
                ),
            }
        }
        // Parent → children edge (skip any child that didn't reload).
        if !meta.children.is_empty() {
            let children: Vec<Entity> = meta
                .children
                .iter()
                .filter_map(|cid| by_run_id.get(cid.as_str()).copied())
                .collect();
            if !children.is_empty() {
                w.entity_mut(*entity).insert(SubAgentChildren {
                    children,
                    max_child_depth: meta.max_child_depth,
                });
            }
            // Keep the serializable child list consistent with the rebuilt
            // component so the next snapshot re-persists the same tree. A reloaded
            // agent always carries `AgentState`.
            w.get_mut::<AgentState>(*entity)
                .expect("a reloaded agent always has AgentState")
                .spawned_children_ids = meta.children.clone();
        }
    }
}

/// Read + parse `<run_dir>/meta.json`, returning `None` if it is missing or
/// invalid.
fn read_meta(run_dir: &Path) -> Option<RunMeta> {
    let text = std::fs::read_to_string(run_dir.join("meta.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether a run's status means it should not be resumed.
fn is_terminal(status: &RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Complete | RunStatus::Cancelled | RunStatus::Error
    )
}

/// Reload one run: spawn it fresh from its blueprint, then overlay the persisted
/// context / stage / totals and preserve the original run metadata.
#[allow(clippy::too_many_arguments)]
fn reload_one(
    world: &mut PipelineWorld,
    tool_service: &CliToolService,
    config: &Config,
    shared_mcp: Arc<Mutex<ToolExecutor>>,
    mcp_tool_defs: &[Tool],
    hub: &InteractionHub,
    meta: &RunMeta,
    run_dir: &Path,
    now_secs: i64,
    subagent_tx: &UnboundedSender<SubAgentOp>,
) -> Result<Entity, String> {
    let args = SpawnArgs {
        run_id: meta.run_id.clone(),
        blueprint_path: meta.agent_path.clone(),
        task: meta.task.clone(),
        // Region seed content isn't replayed on reload: the window is restored
        // from the persisted context snapshot after build_agent, so re-seeding
        // would be redundant (and could double up content).
        regions: Default::default(),
        model: meta.model.clone(),
        workdir: meta.workdir.clone(),
        metadata: meta.metadata.clone(),
        callback_url: meta.callback_url.clone(),
        callback_secret: meta.callback_secret.clone(),
        // Launch overrides aren't persisted; a reloaded run reverts to its
        // blueprint's own tool policy (safe-side: more prompting, never less).
        yolo: false,
        allow: Vec::new(),
        max_depth: None,
        parent_run_id: meta.parent_run_id.clone(),
    };
    let entity = build_agent_for_reload(
        world.world_mut(),
        tool_service,
        config,
        shared_mcp,
        mcp_tool_defs,
        hub,
        &args,
        now_secs,
        subagent_tx.clone(),
    )?;

    // Restore the persisted context (if any), stage, iteration, and token totals.
    let snapshot = std::fs::read_to_string(run_dir.join("context.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<ContextSnapshot>(&s).ok())
        .unwrap_or_else(|| ContextSnapshot {
            stage_name: meta.current_stage.clone(),
            total_tokens: 0,
            max_tokens: 0,
            regions: Vec::new(),
        });
    let totals = TokenTotals {
        prompt_tokens: meta.prompt_tokens,
        completion_tokens: meta.completion_tokens,
        cached_tokens: meta.cached_tokens,
        cache_write_tokens: meta.cache_write_tokens,
        tool_calls: meta.tool_calls,
    };
    restore_agent(
        world.world_mut(),
        entity,
        &snapshot,
        meta.stage_index,
        meta.iteration,
        totals,
    );

    // `build_agent` stamps fresh run metadata; preserve the original identity.
    {
        let mut md = world
            .world_mut()
            .get_mut::<RunMetadata>(entity)
            .expect("build_agent attached run metadata");
        md.started_at = meta.started_at;
        md.title = meta.title.clone();
        md.callback_url = meta.callback_url.clone();
        md.callback_secret = meta.callback_secret.clone();
        // `parent_run_id` was already restored via `args` into build_agent's metadata.
    }

    // If this run was parked at a stage-boundary interaction point (e.g.
    // plan_approval), re-present it in the *waiting* state rather than the default
    // `Active` + `ReadyToInfer` restore — so the open prompt survives the restart
    // instead of being dropped and re-inferred (issue #38). A missing/malformed
    // sidecar, or a blueprint that no longer matches, leaves the default restore.
    if let Some(state) = std::fs::read_to_string(run_dir.join("interactions.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<InteractionPointState>(&s).ok())
    {
        leviath_runtime::interaction_points::restore_interaction_point(
            world.world_mut(),
            entity,
            state,
        );
    }

    Ok(entity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_runtime::ProviderRegistry;
    use leviath_runtime::components::AgentStatus;
    use leviath_runtime::inference_pool::InferencePoolConfig;
    use tokio::runtime::Handle;

    fn sub_tx() -> UnboundedSender<SubAgentOp> {
        tokio::sync::mpsc::unbounded_channel().0
    }

    struct FakeProvider;
    #[async_trait::async_trait]
    impl leviath_providers::Provider for FakeProvider {
        async fn infer(
            &self,
            _r: leviath_providers::InferenceRequest,
        ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
            Err(leviath_providers::ProviderError::Other("t".to_string()))
        }
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            1000
        }
        fn name(&self) -> &str {
            "fake"
        }
        fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    fn test_world() -> (PipelineWorld, Arc<CliToolService>) {
        let cli = Arc::new(CliToolService::new());
        let mut registry = ProviderRegistry::new();
        for p in ["anthropic", "openai", "ollama"] {
            registry.register(p.to_string(), Arc::new(FakeProvider));
        }
        let world = PipelineWorld::new(
            registry,
            cli.clone(),
            InferencePoolConfig::new(),
            std::env::temp_dir(),
            Handle::current(),
        );
        (world, cli)
    }

    fn coder_manifest() -> String {
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../agents/coder/agent.leviath"),
        )
        .expect("read coder manifest")
    }

    /// Write a `<runs_dir>/<run_id>/meta.json` (+ optional context.json) for a run
    /// whose blueprint lives at `agent_path`.
    fn write_run(
        runs_dir: &Path,
        run_id: &str,
        agent_path: &str,
        status: RunStatus,
        context: Option<&ContextSnapshot>,
    ) {
        write_run_tree(
            runs_dir,
            run_id,
            agent_path,
            status,
            context,
            None,
            &[],
            0,
            0,
        );
    }

    /// Like [`write_run`], but with explicit tree links so recovery's re-linking
    /// pass can be exercised.
    #[allow(clippy::too_many_arguments)]
    fn write_run_tree(
        runs_dir: &Path,
        run_id: &str,
        agent_path: &str,
        status: RunStatus,
        context: Option<&ContextSnapshot>,
        parent_run_id: Option<&str>,
        children: &[&str],
        depth: usize,
        max_child_depth: usize,
    ) {
        let dir = runs_dir.join(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = RunMeta {
            run_id: run_id.to_string(),
            agent_name: "coder".to_string(),
            agent_path: agent_path.to_string(),
            task: "resume me".to_string(),
            model: None,
            pid: 0,
            status,
            current_stage: "implement".to_string(),
            stage_index: 0,
            num_stages: 1,
            iteration: 5,
            prompt_tokens: 42,
            completion_tokens: 7,
            cached_tokens: 0,
            cache_write_tokens: 0,
            tool_calls: 3,
            workdir: std::env::temp_dir().to_string_lossy().to_string(),
            started_at: 111,
            updated_at: 222,
            error: None,
            title: Some("Resume Me".to_string()),
            metadata: std::collections::HashMap::new(),
            callback_url: Some("http://cb".to_string()),
            callback_secret: None,
            parent_run_id: parent_run_id.map(str::to_string),
            children: children.iter().map(|s| s.to_string()).collect(),
            depth,
            max_child_depth,
        };
        std::fs::write(dir.join("meta.json"), serde_json::to_string(&meta).unwrap()).unwrap();
        if let Some(ctx) = context {
            std::fs::write(
                dir.join("context.json"),
                serde_json::to_string(ctx).unwrap(),
            )
            .unwrap();
        }
    }

    fn agent_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("agent.leviath"), coder_manifest()).unwrap();
        dir
    }

    #[tokio::test]
    async fn reloads_nonterminal_runs_and_restores_state() {
        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let runs = tempfile::tempdir().unwrap();

        // A running snapshot with real context.
        let ctx = ContextSnapshot {
            stage_name: "implement".to_string(),
            total_tokens: 4,
            max_tokens: 100_000,
            regions: vec![leviath_core::run_meta::RegionSnapshot {
                name: "conversation".to_string(),
                kind: "clearable".to_string(),
                current_tokens: 4,
                max_tokens: 100_000,
                entries: vec![leviath_core::run_meta::RegionEntrySnapshot {
                    content: "earlier turn".to_string(),
                    tokens: 4,
                    kind: leviath_core::region::EntryKind::UserMessage,
                    metadata: None,
                    key: None,
                }],
            }],
        };
        write_run(
            runs.path(),
            "run-live",
            manifest.to_str().unwrap(),
            RunStatus::Running,
            Some(&ctx),
        );
        // A completed run — must be skipped.
        write_run(
            runs.path(),
            "run-done",
            manifest.to_str().unwrap(),
            RunStatus::Complete,
            None,
        );

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            runs.path(),
            999,
            &sub_tx(),
        );

        assert_eq!(restored.len(), 1);
        let (run_id, entity) = &restored[0];
        assert_eq!(run_id, "run-live");
        assert_eq!(world.agent_status(*entity), Some(AgentStatus::Active));
        // Iteration + preserved metadata restored.
        let md = world.world().get::<RunMetadata>(*entity).unwrap();
        assert_eq!(md.started_at, 111);
        assert_eq!(md.title.as_deref(), Some("Resume Me"));
        assert_eq!(md.callback_url.as_deref(), Some("http://cb"));
        let totals = world.world().get::<TokenTotals>(*entity).unwrap();
        assert_eq!(totals.prompt_tokens, 42);
        assert_eq!(totals.tool_calls, 3);
    }

    /// A temp agent dir holding the `software-engineer` manifest, whose stage 0
    /// (`plan`) is an `interactive_points` stage with a `plan_approval` point.
    fn interactive_agent_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let manifest = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../agents/software-engineer/agent.leviath"),
        )
        .expect("read software-engineer manifest");
        std::fs::write(dir.path().join("agent.leviath"), manifest).unwrap();
        dir
    }

    #[tokio::test]
    async fn reload_resumes_a_blocked_interaction_point_in_the_waiting_state() {
        let agent = interactive_agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let runs = tempfile::tempdir().unwrap();

        // A run parked at the plan_approval interaction point (stage 0 = plan)...
        write_run(
            runs.path(),
            "run-await",
            manifest.to_str().unwrap(),
            RunStatus::WaitingInput,
            None,
        );
        // ...plus the interaction sidecar the daemon wrote while it was blocked.
        std::fs::write(
            runs.path().join("run-await/interactions.json"),
            serde_json::to_string(&InteractionPointState {
                cursor: 0,
                round: 0,
                body: "## Plan\n1. do it".to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        world.insert_interaction_hub(hub.clone()); // restore reads the hub resource
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            runs.path(),
            999,
            &sub_tx(),
        );

        assert_eq!(restored.len(), 1);
        let (run_id, entity) = &restored[0];
        assert_eq!(run_id, "run-await");
        // Re-armed in the *waiting* state (not the default Active), so no inference
        // re-issues and the open prompt isn't dropped — the issue #38 fix.
        assert_eq!(world.agent_status(*entity), Some(AgentStatus::Waiting));
        assert!(
            world
                .world()
                .get::<leviath_runtime::interaction_points::AwaitingInteractionPoint>(*entity)
                .is_some()
        );
        assert!(
            world
                .world()
                .get::<leviath_runtime::pipeline::ReadyToInfer>(*entity)
                .is_none(),
            "the spawn-set ReadyToInfer is cleared so the inference lane won't fire"
        );

        // The prompt was re-opened in the hub, carrying the reviewed plan.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let pending = hub.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, "run-await");
        assert_eq!(pending[0].1.body.as_deref(), Some("## Plan\n1. do it"));
    }

    #[tokio::test]
    async fn reload_restores_actionable_runs_before_blocked_and_skips_terminal() {
        let agent = agent_dir();
        let mpath = agent.path().join("agent.leviath");
        let mpath = mpath.to_str().unwrap();
        let runs = tempfile::tempdir().unwrap();
        // Directory iteration order is unspecified; name the blocked run so it would
        // sort ahead alphabetically, proving the triage (not the filesystem) decides.
        write_run(
            runs.path(),
            "aaa-blocked",
            mpath,
            RunStatus::WaitingInput,
            None,
        );
        write_run(runs.path(), "zzz-active", mpath, RunStatus::Running, None);
        write_run(runs.path(), "mmm-done", mpath, RunStatus::Complete, None);

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            runs.path(),
            999,
            &sub_tx(),
        );

        // Terminal run skipped; the actionable (Running) run is restored first.
        let order: Vec<&str> = restored.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(order, vec!["zzz-active", "aaa-blocked"]);
    }

    #[tokio::test]
    async fn reload_run_pages_in_nonterminal_only() {
        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let mpath = manifest.to_str().unwrap();
        let runs = tempfile::tempdir().unwrap();
        write_run(runs.path(), "live", mpath, RunStatus::Running, None);
        write_run(runs.path(), "done", mpath, RunStatus::Complete, None);

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));

        // A non-terminal run is paged in.
        assert!(
            reload_run(
                &mut world,
                cli.as_ref(),
                &Config::default(),
                mcp.clone(),
                &[],
                &hub,
                "live",
                runs.path(),
                1,
                &sub_tx(),
            )
            .is_some()
        );
        // A terminal run is not.
        assert!(
            reload_run(
                &mut world,
                cli.as_ref(),
                &Config::default(),
                mcp.clone(),
                &[],
                &hub,
                "done",
                runs.path(),
                1,
                &sub_tx(),
            )
            .is_none()
        );
        // A run with no meta on disk is not.
        assert!(
            reload_run(
                &mut world,
                cli.as_ref(),
                &Config::default(),
                mcp,
                &[],
                &hub,
                "no-such-run",
                runs.path(),
                1,
                &sub_tx(),
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn resumes_a_parent_parked_mid_fan_out() {
        use leviath_core::blueprint::{FanOutConfig, WorkerFailurePolicy};
        use leviath_runtime::fanout::{FanOutState, FanOutWaiting};

        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let mpath = manifest.to_str().unwrap();
        let runs = tempfile::tempdir().unwrap();

        // A parent parked mid fan-out: a valid fanout.json alongside its meta.
        write_run(
            runs.path(),
            "parent-fo",
            mpath,
            RunStatus::WaitingInput,
            None,
        );
        let state = FanOutState {
            config: FanOutConfig {
                worker_agent: None,
                worker_stage: Some("w".to_string()),
                worker_query: None,
                merge_stage: None,
                max_workers: 1,
                on_worker_failure: WorkerFailurePolicy::Continue,
                split_prompt: "s".to_string(),
            },
            max_workers: 1,
            pending: vec![],
            // One in-flight worker, referenced by the run-id of another reloaded
            // run so the resolver maps it back to an entity on restore.
            active: vec![("item-1".to_string(), "worker-fo".to_string())],
            summaries: vec![],
            failures: vec![],
        };
        std::fs::write(
            runs.path().join("parent-fo").join("fanout.json"),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();
        // The referenced worker run, so the active worker re-links to a real entity.
        write_run(runs.path(), "worker-fo", mpath, RunStatus::Running, None);

        // A run with a malformed fanout.json → skipped (no FanOutWaiting).
        write_run(runs.path(), "bad-fo", mpath, RunStatus::WaitingInput, None);
        std::fs::write(runs.path().join("bad-fo").join("fanout.json"), b"garbage").unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            runs.path(),
            999,
            &sub_tx(),
        );
        let by_id: std::collections::HashMap<_, _> =
            restored.iter().map(|(r, e)| (r.clone(), *e)).collect();

        // The parent's fan-out waiting state was rebuilt; the malformed one wasn't.
        assert!(
            world
                .world()
                .get::<FanOutWaiting>(by_id["parent-fo"])
                .is_some()
        );
        assert!(
            world
                .world()
                .get::<FanOutWaiting>(by_id["bad-fo"])
                .is_none()
        );
    }

    #[tokio::test]
    async fn rebuilds_parent_child_tree_on_reload() {
        use leviath_runtime::components::{ParentRef, SubAgentChildren};

        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let mpath = manifest.to_str().unwrap();
        let runs = tempfile::tempdir().unwrap();

        // A parent with two children + a child that records its parent + depth.
        write_run_tree(
            runs.path(),
            "parent",
            mpath,
            RunStatus::WaitingInput,
            None,
            None,
            &["child-a", "child-b"],
            0,
            4,
        );
        write_run_tree(
            runs.path(),
            "child-a",
            mpath,
            RunStatus::Running,
            None,
            Some("parent"),
            &[],
            1,
            0,
        );
        write_run_tree(
            runs.path(),
            "child-b",
            mpath,
            RunStatus::Running,
            None,
            Some("parent"),
            &[],
            1,
            0,
        );

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            runs.path(),
            999,
            &sub_tx(),
        );
        assert_eq!(restored.len(), 3);
        let by_id: std::collections::HashMap<_, _> =
            restored.iter().map(|(r, e)| (r.clone(), *e)).collect();
        let parent = by_id["parent"];
        let child_a = by_id["child-a"];
        let child_b = by_id["child-b"];

        // Parent's SubAgentChildren rebuilt with both children + the depth cap.
        let kids = world.world().get::<SubAgentChildren>(parent).unwrap();
        assert_eq!(kids.max_child_depth, 4);
        assert_eq!(kids.children.len(), 2);
        assert!(kids.children.contains(&child_a) && kids.children.contains(&child_b));
        // Each child's ParentRef points back at the parent, at its stored depth.
        let pr = world.world().get::<ParentRef>(child_a).unwrap();
        assert_eq!(pr.parent_entity, parent);
        assert_eq!(pr.parent_agent_id, "parent");
        assert_eq!(pr.depth, 1);
        // The serializable child list is kept in sync for the next snapshot.
        let state = world
            .world()
            .get::<leviath_runtime::components::AgentState>(parent)
            .unwrap();
        assert_eq!(state.spawned_children_ids, vec!["child-a", "child-b"]);
    }

    #[tokio::test]
    async fn relink_skips_children_and_parents_that_did_not_reload() {
        use leviath_runtime::components::{ParentRef, SubAgentChildren};

        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let mpath = manifest.to_str().unwrap();
        let runs = tempfile::tempdir().unwrap();

        // Parent lists a child that is terminal (won't reload) → no SubAgentChildren.
        write_run_tree(
            runs.path(),
            "lonely-parent",
            mpath,
            RunStatus::WaitingInput,
            None,
            None,
            &["gone-child"],
            0,
            2,
        );
        write_run_tree(
            runs.path(),
            "gone-child",
            mpath,
            RunStatus::Complete, // terminal → skipped by recovery
            None,
            Some("lonely-parent"),
            &[],
            1,
            0,
        );
        // Child whose parent is terminal (won't reload) → left unlinked.
        write_run_tree(
            runs.path(),
            "orphan",
            mpath,
            RunStatus::Running,
            None,
            Some("gone-parent"),
            &[],
            1,
            0,
        );
        write_run_tree(
            runs.path(),
            "gone-parent",
            mpath,
            RunStatus::Error,
            None,
            None,
            &["orphan"],
            0,
            2,
        );

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            runs.path(),
            999,
            &sub_tx(),
        );
        // Only the two non-terminal runs reload.
        assert_eq!(restored.len(), 2);
        let by_id: std::collections::HashMap<_, _> =
            restored.iter().map(|(r, e)| (r.clone(), *e)).collect();
        // Parent listed a child that didn't reload → no SubAgentChildren attached.
        assert!(
            world
                .world()
                .get::<SubAgentChildren>(by_id["lonely-parent"])
                .is_none()
        );
        // Orphan's parent didn't reload → no ParentRef attached.
        assert!(world.world().get::<ParentRef>(by_id["orphan"]).is_none());
    }

    #[tokio::test]
    async fn reload_without_context_json_still_resumes() {
        let agent = agent_dir();
        let manifest = agent.path().join("agent.leviath");
        let runs = tempfile::tempdir().unwrap();
        write_run(
            runs.path(),
            "run-nocontext",
            manifest.to_str().unwrap(),
            RunStatus::WaitingInput,
            None, // no context.json
        );

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        let restored = reload_persisted_agents(
            &mut world,
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            runs.path(),
            999,
            &sub_tx(),
        );
        assert_eq!(restored.len(), 1);
        assert!(world.world().get::<TokenTotals>(restored[0].1).is_some());
    }

    #[tokio::test]
    async fn skips_missing_dir_junk_and_unreloadable_runs() {
        // A runs dir that doesn't exist → empty.
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(ToolExecutor::new()));
        assert!(
            reload_persisted_agents(
                &mut world,
                cli.as_ref(),
                &Config::default(),
                mcp.clone(),
                &[],
                &hub,
                std::path::Path::new("/no/such/runs/dir"),
                1,
                &sub_tx(),
            )
            .is_empty()
        );

        // A runs dir with junk: a dir without meta.json, a dir with corrupt
        // meta.json, and a non-terminal run pointing at a missing blueprint.
        let runs = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(runs.path().join("no-meta")).unwrap();
        let corrupt = runs.path().join("corrupt");
        std::fs::create_dir_all(&corrupt).unwrap();
        std::fs::write(corrupt.join("meta.json"), "not json").unwrap();
        write_run(
            runs.path(),
            "run-badpath",
            "/no/such/agent.leviath",
            RunStatus::Running,
            None,
        );

        let restored = reload_persisted_agents(
            &mut world,
            cli.as_ref(),
            &Config::default(),
            mcp,
            &[],
            &hub,
            runs.path(),
            1,
            &sub_tx(),
        );
        assert!(restored.is_empty()); // all skipped, none fatal
    }

    #[tokio::test]
    async fn fake_provider_methods_are_exercised() {
        use leviath_providers::Provider;
        let p = FakeProvider;
        assert_eq!(p.name(), "fake");
        assert_eq!(p.count_tokens("t", "m").await, 1);
        assert_eq!(p.max_context_tokens("m"), 1000);
        let _ = p.capabilities("m");
        assert!(
            p.infer(leviath_providers::InferenceRequest {
                system: vec![],
                messages: vec![],
                model: "m".to_string(),
                max_tokens: 1,
                temperature: 0.0,
                tools: vec![],
                extra: serde_json::Value::Null,
            })
            .await
            .is_err()
        );
    }

    #[test]
    fn is_terminal_covers_all_statuses() {
        assert!(is_terminal(&RunStatus::Complete));
        assert!(is_terminal(&RunStatus::Cancelled));
        assert!(is_terminal(&RunStatus::Error));
        assert!(!is_terminal(&RunStatus::Running));
        assert!(!is_terminal(&RunStatus::WaitingInput));
    }
}
