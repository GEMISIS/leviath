use super::*;
use crate::config::Config;
use crate::daemon::starter::testing::{manifest_in, run_on_disk, starter, task_request};
use crate::test_support::FakeProvider;
use leviath_runtime::ProviderRegistry;
use leviath_runtime::components::{ParentRef, SubAgentChildren};
use leviath_runtime::insert::RunSpecC;
use leviath_runtime::runfile::{RunFileReader, RunFileWriter};
use leviath_runtime::spec::env::Caller;
use leviath_runtime::state::RunState;
use std::path::PathBuf;
use std::sync::Arc;

/// The fake `anthropic` provider the coder blueprint names, as a registry.
fn registry() -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    registry.register(
        "anthropic".to_string(),
        Arc::new(FakeProvider::new().context_window(100_000)),
    );
    registry
}

/// The coder blueprint, in `dir`.
fn coder(dir: &Path) -> PathBuf {
    manifest_in(dir, &crate::test_support::inline_coder_manifest())
}

/// A world over `starter`'s providers and tools.
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

/// The run file of `run_id` under `runs`.
fn run_file(runs: &Path, run_id: &str) -> PathBuf {
    runs.join(run_id).join(leviath_core::files::RUN_FILE)
}

/// Record one more step on `run_id`'s file: its state as `change` leaves it.
fn change(runs: &Path, run_id: &str, change: impl FnOnce(&mut RunState)) {
    let mut writer =
        RunFileWriter::open(&run_file(runs, run_id), Default::default()).expect("the file opens");
    let mut next = writer.state().clone();
    change(&mut next);
    writer
        .record(next, 1, Vec::new())
        .expect("the step is written");
}

/// The status `run_id`'s file holds last.
fn status_on_disk(runs: &Path, run_id: &str) -> RunStatus {
    RunFileReader::open(&run_file(runs, run_id))
        .and_then(|r| r.latest_state())
        .expect("the run file reads")
        .status
}

/// The entity `run_id` was placed as, in `world`.
fn entity_of(world: &mut PipelineWorld, run_id: &str) -> Option<Entity> {
    let ecs = world.world_mut();
    let mut runs = ecs.query::<(Entity, &RunSpecC)>();
    runs.iter(ecs)
        .find(|(_, spec)| spec.0.run_id.as_str() == run_id)
        .map(|(e, _)| e)
}

#[tokio::test]
async fn a_recorded_run_comes_back_with_its_own_spec_and_state() {
    let agent = tempfile::tempdir().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let run_id = run_on_disk(
        Config::default(),
        registry(),
        runs.path(),
        &coder(agent.path()),
    );
    change(runs.path(), &run_id, |s| {
        s.title = Some("kept across".to_string())
    });

    let starter = starter(Config::default(), registry(), runs.path());
    let mut world = world_for(&starter);
    let recovered = resume_all(&mut world, &starter, runs.path());

    assert_eq!(recovered.reloaded.len(), 1);
    assert_eq!(recovered.reloaded[0].0, run_id);
    let entity = entity_of(&mut world, &run_id).expect("the run is in the world");
    let on_disk = RunFileReader::open(&run_file(runs.path(), &run_id)).unwrap();
    assert_eq!(
        *world.world().get::<RunSpecC>(entity).unwrap().0,
        *on_disk.spec(),
        "nothing was resolved again"
    );
    let state = leviath_runtime::state::inspect::inspect(world.world(), entity)
        .expect("the run reads back");
    assert_eq!(state.title.as_deref(), Some("kept across"));
}

#[tokio::test]
async fn finished_failed_and_cancelled_runs_stay_on_disk() {
    let agent = tempfile::tempdir().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let manifest = coder(agent.path());
    for status in [
        RunStatus::Complete,
        RunStatus::Error("boom".to_string()),
        RunStatus::Cancelled,
    ] {
        let run_id = run_on_disk(Config::default(), registry(), runs.path(), &manifest);
        change(runs.path(), &run_id, |s| s.status = status);
    }

    let starter = starter(Config::default(), registry(), runs.path());
    let mut world = world_for(&starter);
    assert!(
        resume_all(&mut world, &starter, runs.path())
            .reloaded
            .is_empty()
    );
}

/// A run whose provider is gone from this machine is refused with what
/// changed, and the refusal is recorded on its file so the run ends there
/// rather than sitting as one that never moves.
#[tokio::test]
async fn a_run_that_cannot_be_bound_here_is_recorded_as_failed() {
    let agent = tempfile::tempdir().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let run_id = run_on_disk(
        Config::default(),
        registry(),
        runs.path(),
        &coder(agent.path()),
    );

    let starter = starter(Config::default(), ProviderRegistry::new(), runs.path());
    let mut world = world_for(&starter);
    let recovered = resume_all(&mut world, &starter, runs.path());

    assert!(recovered.reloaded.is_empty());
    match status_on_disk(runs.path(), &run_id) {
        RunStatus::Error(why) => assert!(why.contains("anthropic"), "{why}"),
        other => panic!("the refusal is not on the run's file: {other:?}"),
    }
    // A refused run is not paged in on demand either.
    assert!(reload_run(&mut world, &starter, &run_id).is_none());
}

/// A refusal that cannot be written down is logged, and the run is still
/// left out.
#[tokio::test]
async fn a_refusal_that_cannot_be_recorded_still_leaves_the_run_out() {
    let agent = tempfile::tempdir().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let run_id = run_on_disk(
        Config::default(),
        registry(),
        runs.path(),
        &coder(agent.path()),
    );
    let found = read_run(&runs.path().join(&run_id)).expect("the run reads");
    // The file goes away between reading it and recording the refusal.
    std::fs::remove_file(run_file(runs.path(), &run_id)).unwrap();

    let starter = starter(Config::default(), ProviderRegistry::new(), runs.path());
    let mut world = world_for(&starter);
    assert!(resume_one(&mut world, &starter, found).is_err());
}

#[tokio::test]
async fn what_is_not_a_readable_run_is_passed_over() {
    let runs = tempfile::tempdir().unwrap();
    // A stray file, a directory with no run file, and a run file that is not one.
    std::fs::write(runs.path().join("stray.txt"), "x").unwrap();
    std::fs::create_dir_all(runs.path().join("empty")).unwrap();
    std::fs::create_dir_all(runs.path().join("garbage")).unwrap();
    std::fs::write(run_file(runs.path(), "garbage"), b"not a run file").unwrap();

    let starter = starter(Config::default(), registry(), runs.path());
    let mut world = world_for(&starter);
    let recovered = resume_all(&mut world, &starter, runs.path());
    assert!(recovered.reloaded.is_empty());
    assert!(reload_run(&mut world, &starter, "garbage").is_none());
    assert!(reload_run(&mut world, &starter, "no-such-run").is_none());

    // No runs directory at all brings back nothing.
    let gone = runs.path().join("gone");
    assert!(resume_all(&mut world, &starter, &gone).reloaded.is_empty());
}

#[tokio::test]
async fn reload_run_pages_in_a_cancelled_run_paused_but_not_a_finished_one() {
    let agent = tempfile::tempdir().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let manifest = coder(agent.path());
    let cancelled = run_on_disk(Config::default(), registry(), runs.path(), &manifest);
    change(runs.path(), &cancelled, |s| s.status = RunStatus::Cancelled);
    let complete = run_on_disk(Config::default(), registry(), runs.path(), &manifest);
    change(runs.path(), &complete, |s| s.status = RunStatus::Complete);
    let failed = run_on_disk(Config::default(), registry(), runs.path(), &manifest);
    change(runs.path(), &failed, |s| {
        s.status = RunStatus::Error("boom".to_string());
    });
    let live = run_on_disk(Config::default(), registry(), runs.path(), &manifest);

    let starter = starter(Config::default(), registry(), runs.path());
    let mut world = world_for(&starter);
    assert!(reload_run(&mut world, &starter, &complete).is_none());
    assert!(reload_run(&mut world, &starter, &failed).is_none());
    let paged = reload_run(&mut world, &starter, &cancelled).expect("a cancelled run comes back");
    assert_eq!(
        world.agent_status(paged),
        Some(leviath_runtime::components::AgentStatus::Paused),
        "paused, so resuming it carries on"
    );
    assert!(reload_run(&mut world, &starter, &live).is_some());

    // A live run this machine can no longer bind is not paged in.
    let unbound = run_on_disk(Config::default(), registry(), runs.path(), &manifest);
    let bare = crate::daemon::starter::testing::starter(
        Config::default(),
        ProviderRegistry::new(),
        runs.path(),
    );
    assert!(reload_run(&mut world, &bare, &unbound).is_none());
}

/// Children come back linked to the run that started them, and a parent to
/// the children it recorded. A child whose parent did not come back is left
/// unlinked rather than pointed at nothing.
#[tokio::test]
async fn the_tree_of_runs_is_linked_back_together() {
    let agent = tempfile::tempdir().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let manifest = coder(agent.path());
    let parent = run_on_disk(Config::default(), registry(), runs.path(), &manifest);
    let recorder = starter(Config::default(), registry(), runs.path());
    let child_of = |parent: &str| {
        let request = task_request(&manifest, "a child");
        let env = recorder.env_for(&request, recorder.config.current());
        let caller = Caller::Child {
            parent: leviath_runtime::spec::names::RunId::new(parent).unwrap(),
            policy: leviath_runtime::spec::launch::LaunchPolicy {
                unattended: Default::default(),
                allow: Vec::new(),
                max_depth: 4,
                seed_commands: true,
                capture_model_input: false,
            },
            depth: 0,
        };
        crate::daemon::block_on::block_on(recorder.start_with(env, request, caller))
            .expect("the child starts")
            .spec
            .run_id
            .clone()
    };
    let child = child_of(&parent);
    let orphan = child_of("gone-parent");
    change(runs.path(), &parent, |s| s.children.push(child.clone()));

    let starter = starter(Config::default(), registry(), runs.path());
    let mut world = world_for(&starter);
    let recovered =
        crate::test_support::with_tracing(|| resume_all(&mut world, &starter, runs.path()));
    assert_eq!(recovered.reloaded.len(), 3);

    let parent_entity = entity_of(&mut world, &parent).unwrap();
    let child_entity = entity_of(&mut world, child.as_str()).unwrap();
    let orphan_entity = entity_of(&mut world, orphan.as_str()).unwrap();
    let link = world
        .world()
        .get::<ParentRef>(child_entity)
        .expect("the child is linked");
    assert_eq!(link.parent_entity, parent_entity);
    assert_eq!(link.depth, 1);
    assert_eq!(
        world
            .world()
            .get::<SubAgentChildren>(parent_entity)
            .expect("the parent knows its child")
            .children,
        [child_entity]
    );
    assert!(world.world().get::<ParentRef>(orphan_entity).is_none());
}

/// A run that had put questions to a person comes back counting them, so a
/// question it asks again gets an id of its own.
#[tokio::test]
async fn a_resumed_run_counts_the_questions_it_already_asked() {
    let agent = tempfile::tempdir().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let run_id = run_on_disk(
        Config::default(),
        registry(),
        runs.path(),
        &coder(agent.path()),
    );
    let mut writer =
        RunFileWriter::open(&run_file(runs.path(), &run_id), Default::default()).unwrap();
    let mut next = writer.state().clone();
    next.title = Some("asked".to_string());
    writer
        .record(
            next,
            1,
            vec![leviath_runtime::state::RunEvent::Answered {
                id: "q-1".to_string(),
                answer: "yes".to_string(),
            }],
        )
        .unwrap();

    let found = read_run(&runs.path().join(&run_id)).expect("the run reads");
    assert_eq!(found.asked, 1);
}

/// `src`'s files and directories, copied under `dst`.
#[cfg(feature = "legacy-runs")]
fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap().flatten() {
        let to = dst.join(entry.file_name());
        match entry.path().is_dir() {
            true => copy_dir(&entry.path(), &to),
            false => {
                std::fs::copy(entry.path(), &to).unwrap();
            }
        }
    }
}

/// A run directory in the older layout becomes a run file the first time
/// the daemon looks at it, and is then read like any other run. One that is
/// already a run file, and a directory that is no run at all, are left as
/// they are.
#[cfg(feature = "legacy-runs")]
#[tokio::test]
async fn an_old_run_directory_is_converted_on_first_load() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("leviath-legacy-runs")
        .join("tests")
        .join("fixtures");
    let runs = tempfile::tempdir().unwrap();
    copy_dir(&fixtures.join("mid-tool-batch"), &runs.path().join("old"));
    std::fs::create_dir_all(runs.path().join("empty")).unwrap();
    // A directory that claims the older layout and cannot be read as it.
    std::fs::create_dir_all(runs.path().join("broken")).unwrap();
    std::fs::write(runs.path().join("broken").join("meta.json"), "not json").unwrap();

    crate::test_support::with_tracing(|| {
        convert_old_runs(runs.path(), Some(&fixtures.join("agents")));
    });

    let old = runs.path().join("old");
    assert!(
        old.join("legacy").join("meta.json").is_file(),
        "the old files are kept aside"
    );
    let found = read_run(&old).expect("the converted run reads as a run file");
    assert!(!found.spec.run_id.as_str().is_empty());
    assert!(runs.path().join("broken").join("meta.json").is_file());

    // Converting again finds nothing to do.
    convert_old_run(&old, None);
    assert!(read_run(&old).is_some());
    // A runs directory that is not there converts nothing.
    convert_old_runs(&runs.path().join("gone"), None);
}

/// A finished run converted from the older layout still hands back its
/// answer: the sidecar is kept aside under `legacy/`, and the run file holds
/// the same bytes.
#[cfg(feature = "legacy-runs")]
#[test]
fn a_converted_run_answers_from_its_run_file() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("leviath-legacy-runs")
        .join("tests")
        .join("fixtures")
        .join("real-finished");
    let runs = tempfile::tempdir().unwrap();
    let dir = runs.path().join("done");
    copy_dir(&fixture, &dir);
    convert_old_run(&dir, None);

    let kept = std::fs::read_to_string(dir.join("legacy").join(leviath_core::FINAL_OUTPUT_FILE))
        .expect("the sidecar is kept aside");
    let meta = crate::runstate::read_meta_from(&dir).expect("the run file reads");
    let answer = crate::runstate::read_final_output_in(&dir, &meta).expect("the answer");
    assert_eq!(answer.content, kept);
}
