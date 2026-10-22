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
    let manifest = manifest_in(agent, BLUEPRINT);
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
