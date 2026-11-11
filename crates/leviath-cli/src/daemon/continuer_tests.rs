//! The daemon's half of continuing a finished run: read it back from its
//! file once the lane has written it, fork it when asked, reopen, reseed and
//! bind it, and place it.

use std::path::Path;

use leviath_runtime::ProviderRegistry;
use leviath_runtime::continuation::ContinueMessage;
use leviath_runtime::host::{ContinueJob, ContinueRequest};
use leviath_runtime::runfile::RunFileWriter;
use leviath_runtime::state::RunStatus;

use super::*;
use crate::config::Config;
use crate::daemon::starter::testing::{manifest_in, run_on_disk, starter};
use crate::test_support::FakeProvider;

const BLUEPRINT: &str = r#"[blueprint]
name = "again"
version = "0.0.0"
description = "Continued."

[graph]
inputs = [{ name = "task", type = { kind = "text", multiline = true }, binds = [{ region = "task" }] }]

[graph.layout]
total_budget_tokens = 50000
regions = [
    { name = "task", kind = "pinned", budget = 2000 },
    { name = "refs", kind = "pinned", budget = 2000, seed = { literal = "the sheet" }, on_continue = "reseed" },
    { name = "conversation", kind = { kind = "sliding_window", max_items = 40 }, budget = 20000 },
]

[[graph.stages]]
name = "work"
model = { models = [{ provider = "anthropic", model = "m" }] }
system_prompt = "Work."
"#;

fn registry() -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    registry.register(
        "anthropic".to_string(),
        Arc::new(FakeProvider::new().replying("done").context_window(100_000)),
    );
    registry
}

/// A run of the blueprint under `runs`, recorded as `status`.
fn run_in(agent: &Path, runs: &Path, status: RunStatus) -> String {
    run_of(agent, runs, status, BLUEPRINT)
}

/// A run of `blueprint` under `runs`, recorded as `status`.
fn run_of(agent: &Path, runs: &Path, status: RunStatus, blueprint: &str) -> String {
    let manifest = manifest_in(agent, blueprint);
    let run_id = run_on_disk(Config::default(), registry(), runs, &manifest);
    let path = runs.join(&run_id).join(leviath_core::files::RUN_FILE);
    let mut writer = RunFileWriter::open(&path, Default::default()).unwrap();
    let mut next = writer.state().clone();
    next.status = status;
    writer.record(next, 5, Vec::new()).unwrap();
    run_id
}

fn job(run_id: &str, fork: bool) -> ContinueJob {
    ContinueJob {
        run_id: run_id.to_string(),
        request: ContinueRequest {
            message: ContinueMessage {
                text: "again".to_string(),
                ..Default::default()
            },
            fork,
        },
        written: Box::pin(async {}),
    }
}

fn world_for(starter: &DaemonStarter) -> PipelineWorld {
    PipelineWorld::new(
        starter.providers.registry(),
        starter.tool_service.clone(),
        leviath_runtime::inference_pool::InferencePoolConfig::new(),
        1,
        Some(starter.runs_dir.clone()),
        tokio::runtime::Handle::current(),
    )
}

fn code(refused: Result<leviath_runtime::host::PlaceContinued, SpawnIssues>) -> IssueCode {
    match refused {
        Ok(_) => panic!("the continuation was refused"),
        Err(issues) => issues.0[0].code,
    }
}

#[tokio::test]
async fn a_finished_run_is_reopened_and_placed_under_its_own_id() {
    let (agent, runs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let run_id = run_in(agent.path(), runs.path(), RunStatus::Complete);
    let starter = Arc::new(starter(Config::default(), registry(), runs.path()));
    let mut continuer = continuer(starter.clone());
    let place = continuer(job(&run_id, false)).await.unwrap();
    let mut world = world_for(&starter);
    let (placed, _) = place(&mut world).unwrap();
    assert_eq!(placed, run_id);
}

#[tokio::test]
async fn a_fork_is_written_as_a_new_run_and_placed_under_that() {
    let (agent, runs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let run_id = run_in(agent.path(), runs.path(), RunStatus::Complete);
    let starter = Arc::new(starter(Config::default(), registry(), runs.path()));
    let place = prepare(starter.clone(), job(&run_id, true)).await.unwrap();
    let mut world = world_for(&starter);
    let (placed, _) = place(&mut world).unwrap();
    assert_ne!(placed, run_id);
    assert!(
        runs.path()
            .join(&placed)
            .join(leviath_core::files::RUN_FILE)
            .is_file()
    );
}

#[tokio::test]
async fn a_run_that_cannot_be_continued_says_why() {
    let (agent, runs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let starter = Arc::new(starter(Config::default(), registry(), runs.path()));
    // No such run.
    assert_eq!(
        code(prepare(starter.clone(), job("nope", false)).await),
        IssueCode::Missing
    );
    // A file that does not read.
    std::fs::create_dir_all(runs.path().join("junk")).unwrap();
    std::fs::write(
        runs.path().join("junk").join(leviath_core::files::RUN_FILE),
        b"LVR2 but not really",
    )
    .unwrap();
    assert_eq!(
        code(prepare(starter.clone(), job("junk", false)).await),
        IssueCode::Unavailable
    );
    // Still going, in place or forked.
    let going = run_in(agent.path(), runs.path(), RunStatus::Active);
    for fork in [false, true] {
        assert_eq!(
            code(prepare(starter.clone(), job(&going, fork)).await),
            IssueCode::Conflict
        );
    }
    // Finished, on a machine that no longer has its provider.
    let done = run_in(agent.path(), runs.path(), RunStatus::Complete);
    let bare = Arc::new(super::super::starter::testing::starter(
        Config::default(),
        ProviderRegistry::new(),
        runs.path(),
    ));
    assert!(prepare(bare, job(&done, false)).await.is_err());
}

/// A run whose conversation is summarized on continuing, on a compaction
/// model no provider here serves, is refused before a fork is written.
#[tokio::test]
async fn a_summary_no_provider_here_can_write_refuses_the_continuation() {
    let (agent, runs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let blueprint = BLUEPRINT
        .replace(
            "[graph.layout]",
            "compaction = { model = { provider = \"ghost\", model = \"m\" } }\n\n[graph.layout]",
        )
        .replace(
            "budget = 20000 }",
            "budget = 20000, on_continue = \"compact\" }",
        );
    let run_id = run_of(agent.path(), runs.path(), RunStatus::Complete, &blueprint);
    let starter = Arc::new(starter(Config::default(), registry(), runs.path()));
    assert_eq!(
        code(prepare(starter.clone(), job(&run_id, true)).await),
        IssueCode::Unavailable
    );
    assert_eq!(std::fs::read_dir(runs.path()).unwrap().count(), 1);
}

/// A run continued after its own MCP server went idle and was torn down
/// connects the server again before it is bound, and holds it once placed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_continued_after_its_mcp_server_went_idle_connects_it_again() {
    let (_stub_dir, stub) = crate::daemon::starter::testing::stub_server_py();
    let (agent, runs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let manifest = crate::daemon::starter::testing::blueprint_with_mcp(agent.path(), &stub);
    let with_fake = || {
        let mut registry = registry();
        registry.register(
            "fake".to_string(),
            Arc::new(FakeProvider::new().replying("done").context_window(100_000)),
        );
        registry
    };
    // Started as the daemon starts a run, its server connected first so the
    // run records the server's tools, and then finished.
    let first = starter(Config::default(), with_fake(), runs.path());
    let started = first
        .spawn(
            crate::daemon::starter::testing::task_request(&manifest, "t"),
            leviath_runtime::spec::env::Caller::TopLevel,
        )
        .await
        .expect("the run starts");
    let run_id = started.spec.run_id.to_string();
    let server = crate::daemon::starter::mcp_configs(&started.spec.graph).remove(0);
    drop(started);
    let path = runs
        .path()
        .join(&run_id)
        .join(leviath_core::files::RUN_FILE);
    let mut writer = RunFileWriter::open(&path, Default::default()).unwrap();
    let mut next = writer.state().clone();
    next.status = RunStatus::Complete;
    writer.record(next, 5, Vec::new()).unwrap();
    drop(writer);

    // A pool with nothing connected: the run's server is gone.
    let starter = Arc::new(starter(Config::default(), with_fake(), runs.path()));
    let place = prepare(starter.clone(), job(&run_id, false))
        .await
        .unwrap_or_else(|issues| panic!("the run continues: {issues}"));
    assert_eq!(starter.mcp_pool.leased_holders(&server), 1);
    let mut world = world_for(&starter);
    let (placed, _) = place(&mut world).unwrap();
    assert_eq!(placed, run_id);
    assert_eq!(
        starter.mcp_pool.leased_holders(&server),
        1,
        "the run holds it"
    );
}
