//! Restart recovery: reload persisted non-terminal agents into a fresh world when
//! the daemon starts, so runs interrupted by a stop/crash resume where they left
//! off — critically, any agent that was mid-inference re-issues that inference
//! (the reloaded agent is `ReadyToInfer`), rather than being lost.
//!
//! For each `<runs_dir>/<run_id>/meta.json` whose status is non-terminal, this
//! loads the blueprint (via [`build_agent`], reusing the spawn path), restores the
//! persisted context / stage / iteration / token totals via
//! [`leviath_runtime::restore::restore_agent`], and preserves the original run
//! metadata. Anything unreadable or un-reloadable is skipped (logged), never fatal.

use std::path::Path;
use std::sync::Arc;

use bevy_ecs::entity::Entity;
use leviath_core::run_meta::{ContextSnapshot, RunMeta, RunStatus};
use leviath_mcp::ToolExecutor;
use leviath_providers::Tool;
use leviath_runtime::host::SpawnArgs;
use leviath_runtime::interaction_hub::InteractionHub;
use leviath_runtime::persistence::{RunMetadata, TokenTotals};
use leviath_runtime::restore::restore_agent;
use leviath_runtime::world::PipelineWorld;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::daemon::spawn::build_agent;
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
) -> Vec<(String, Entity)> {
    let mut restored = Vec::new();
    let Ok(dir_entries) = std::fs::read_dir(runs_dir) else {
        return restored; // no runs dir yet — nothing to recover
    };
    for dir_entry in dir_entries.flatten() {
        let run_dir = dir_entry.path();
        let Some(meta) = read_meta(&run_dir) else {
            continue; // no meta.json, or unreadable/unparseable
        };
        if is_terminal(&meta.status) {
            continue; // completed / cancelled / errored — don't resume
        }
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
        ) {
            Ok(entity) => restored.push((meta.run_id.clone(), entity)),
            Err(e) => {
                tracing::warn!(run_id = %meta.run_id, error = %e, "skipping un-reloadable run");
            }
        }
    }
    restored
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
) -> Result<Entity, String> {
    let args = SpawnArgs {
        run_id: meta.run_id.clone(),
        blueprint_path: meta.agent_path.clone(),
        task: meta.task.clone(),
        model: meta.model.clone(),
        workdir: meta.workdir.clone(),
        metadata: meta.metadata.clone(),
    };
    let entity = build_agent(
        world,
        tool_service,
        config,
        shared_mcp,
        mcp_tool_defs,
        hub,
        &args,
        now_secs,
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
    let mut md = world
        .world_mut()
        .get_mut::<RunMetadata>(entity)
        .expect("build_agent attached run metadata");
    md.started_at = meta.started_at;
    md.title = meta.title.clone();
    md.callback_url = meta.callback_url.clone();
    md.parent_run_id = meta.parent_run_id.clone();

    Ok(entity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_runtime::components::AgentStatus;
    use leviath_runtime::engine::ProviderRegistry;
    use leviath_runtime::inference_pool::InferencePoolConfig;
    use tokio::runtime::Handle;

    struct FakeProvider;
    #[async_trait::async_trait]
    impl leviath_providers::Provider for FakeProvider {
        async fn infer(
            &self,
            _r: leviath_providers::InferenceRequest,
        ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
            Err(leviath_providers::ProviderError::Other("t".to_string()))
        }
        fn count_tokens(&self, _t: &str, _m: &str) -> usize {
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
            parent_run_id: None,
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
        );
        assert!(restored.is_empty()); // all skipped, none fatal
    }

    #[tokio::test]
    async fn fake_provider_methods_are_exercised() {
        use leviath_providers::Provider;
        let p = FakeProvider;
        assert_eq!(p.name(), "fake");
        assert_eq!(p.count_tokens("t", "m"), 1);
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
