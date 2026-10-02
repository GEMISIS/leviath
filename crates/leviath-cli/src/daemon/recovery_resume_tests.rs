//! A run that was waiting on a person when the daemon stopped comes back
//! waiting on the same question: read from its file, bound and placed the way
//! the daemon does it, then driven until the question is open again.

use super::*;
use crate::config::Config;
use crate::daemon::starter::testing::{manifest_in, run_on_disk, starter};
use crate::test_support::FakeProvider;
use leviath_core::interaction::InteractionResponse;
use leviath_runtime::ProviderRegistry;
use leviath_runtime::insert::RunSpecC;
use leviath_runtime::runfile::RunFileWriter;
use leviath_runtime::state::context::ToolCallState;
use leviath_runtime::state::{OpenInteraction, PendingBatch, PipelinePhase, RunState, WaitState};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// A blueprint whose first stage may ask a person a question, and whose
/// second stops at a checkpoint before it moves on.
const ASKER: &str = r#"[blueprint]
name = "asker"
version = "0.0.0"
description = "Asks a person, then stops at a checkpoint."

[graph]
entry = "ask"
inputs = [{ name = "task", type = { kind = "text", multiline = true }, binds = [{ region = "task" }] }]

[graph.layout]
total_budget_tokens = 50000
regions = [
    { name = "task", kind = "pinned", budget = 2000 },
    { name = "conversation", kind = { kind = "sliding_window", max_items = 40 }, budget = 20000 },
]

[[graph.stages]]
name = "ask"
model = { models = [{ provider = "anthropic", model = "m" }] }
tools = ["ask_user_text"]
allow_blocking_tools = true
system_prompt = "Ask."

[[graph.stages]]
name = "plan"
model = { models = [{ provider = "anthropic", model = "m" }] }
system_prompt = "Plan."

[[graph.stages.mode.interactive_points]]
name = "plan_approval"
prompt = "Approve the plan?"
required = true
unattended = "ask"
style = "multiple_choice"
options = ["approve", "abort"]
abort_options = ["abort"]

[[graph.edges]]
name = "plan"
from = "ask"
to = "plan"
"#;

/// The fake `anthropic` provider the blueprint names, answering `done`.
fn registry() -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    registry.register(
        "anthropic".to_string(),
        Arc::new(FakeProvider::new().replying("done").context_window(100_000)),
    );
    registry
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

/// A world over `starter`'s providers, tools and question hub.
fn world_for(starter: &DaemonStarter) -> PipelineWorld {
    let mut world = PipelineWorld::new(
        starter.providers.registry(),
        starter.tool_service.clone(),
        leviath_runtime::inference_pool::InferencePoolConfig::new(),
        1,
        Some(starter.runs_dir.clone()),
        tokio::runtime::Handle::current(),
    );
    world.insert_interaction_hub(starter.hub.clone());
    world
}

/// The entity `run_id` was placed as, in `world`.
fn entity_of(world: &mut PipelineWorld, run_id: &str) -> Entity {
    let ecs = world.world_mut();
    let mut runs = ecs.query::<(Entity, &RunSpecC)>();
    runs.iter(ecs)
        .find(|(_, spec)| spec.0.run_id.as_str() == run_id)
        .map(|(e, _)| e)
        .expect("the run is in the world")
}

/// Drive `world` until `done` holds, for at most half a minute (a loaded
/// machine running the whole suite is slow). Whether it
/// came to hold.
async fn drive_until(
    world: &mut PipelineWorld,
    mut done: impl FnMut(&mut PipelineWorld) -> bool,
) -> bool {
    for _ in 0..1500 {
        let _ = tokio::time::timeout(Duration::from_millis(20), world.run()).await;
        if done(world) {
            return true;
        }
    }
    false
}

/// The questions open for `run_id`, as `(id, request)`.
fn open_for(
    starter: &DaemonStarter,
    run_id: &str,
) -> Vec<leviath_core::interaction::InteractionRequest> {
    starter
        .hub
        .pending()
        .into_iter()
        .filter(|(run, _)| run == run_id)
        .map(|(_, request)| request)
        .collect()
}

/// The state the live run `run_id` is in.
fn live(world: &mut PipelineWorld, run_id: &str) -> RunState {
    let entity = entity_of(world, run_id);
    leviath_runtime::state::inspect::inspect(world.world(), entity).expect("the run reads")
}

/// A run stopped on an `ask_user_text` call comes back asking it again,
/// under an id of its own, waiting on a person as it was; answering it lets
/// the batch finish.
#[tokio::test]
async fn a_question_put_to_a_person_is_asked_again_after_a_restart() {
    let agent = tempfile::tempdir().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let manifest = manifest_in(agent.path(), ASKER);
    let run_id = run_on_disk(Config::default(), registry(), runs.path(), &manifest);
    let old_id = format!("{run_id}-ask_user_text-call_1");
    change(runs.path(), &run_id, |s| {
        s.status = RunStatus::Waiting;
        s.phase = PipelinePhase::AwaitingPerson;
        s.pending = Some(PendingBatch {
            calls: vec![ToolCallState {
                id: "call_1".to_string(),
                name: "ask_user_text".to_string(),
                args: leviath_core::JsonDoc::new(serde_json::json!({"prompt": "What colour?"})),
                thought_signature: None,
            }],
            done: Default::default(),
        });
        s.interactions = vec![OpenInteraction {
            id: old_id.clone(),
            prompt: "What colour?".to_string(),
            options: Vec::new(),
        }];
        s.wait_reason = Some(WaitState::UserPrompt);
    });

    let starter = starter(Config::default(), registry(), runs.path());
    let mut world = world_for(&starter);
    let recovered = resume_all(&mut world, &starter, runs.path());
    assert_eq!(recovered.reloaded.len(), 1);

    let reopened = drive_until(&mut world, |_| open_for(&starter, &run_id).len() == 1).await;
    assert!(reopened, "the question is open again");
    let asked = open_for(&starter, &run_id).remove(0);
    assert_eq!(asked.prompt, "What colour?");
    assert_ne!(asked.id, old_id, "asked again under an id of its own");
    let waiting = drive_until(&mut world, |w| {
        live(w, &run_id).wait_reason == Some(WaitState::UserPrompt)
    })
    .await;
    assert!(waiting, "the run says what it waits on");
    let state = live(&mut world, &run_id);
    assert_eq!(state.status, RunStatus::Waiting);
    assert_eq!(state.phase, PipelinePhase::AwaitingPerson);
    assert_eq!(state.interactions.len(), 1);
    let pending = state.pending.expect("the batch is still in flight");
    assert_eq!(pending.calls.len(), 1);
    assert!(pending.done.is_empty());

    assert!(
        starter
            .hub
            .answer(InteractionResponse::text(asked.id.clone(), "blue"))
    );
    let moved_on = drive_until(&mut world, |w| live(w, &run_id).pending.is_none()).await;
    assert!(moved_on, "the answered batch finishes");
    // The run may already be at its next stage's checkpoint; the question it
    // asked again is settled.
    assert!(open_for(&starter, &run_id).iter().all(|q| q.id != asked.id));
}

/// A run stopped at a stage's checkpoint comes back asking it again, under
/// the same id and over the same document, waiting on a person as it was;
/// approving it lets the run finish.
#[tokio::test]
async fn a_checkpoint_is_asked_again_after_a_restart() {
    let agent = tempfile::tempdir().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let manifest = manifest_in(agent.path(), ASKER);
    let run_id = run_on_disk(Config::default(), registry(), runs.path(), &manifest);
    let point_id = format!("{run_id}-point-plan_approval-0");
    change(runs.path(), &run_id, |s| {
        s.cursor.stage = leviath_runtime::spec::names::StageName::new("plan").unwrap();
        s.status = RunStatus::Waiting;
        s.phase = PipelinePhase::AwaitingPerson;
        s.interactions = vec![OpenInteraction {
            id: point_id.clone(),
            prompt: "Approve the plan?".to_string(),
            options: vec!["approve".to_string(), "abort".to_string()],
        }];
        s.wait_reason = Some(WaitState::InteractionPoint);
        s.point.asking = Some("the plan".to_string());
    });

    let starter = starter(Config::default(), registry(), runs.path());
    let mut world = world_for(&starter);
    assert_eq!(
        resume_all(&mut world, &starter, runs.path()).reloaded.len(),
        1
    );

    let reopened = drive_until(&mut world, |_| open_for(&starter, &run_id).len() == 1).await;
    assert!(reopened, "the checkpoint is open again");
    let asked = open_for(&starter, &run_id).remove(0);
    assert_eq!(asked.id, point_id, "the same checkpoint, so the same id");
    assert_eq!(asked.body.as_deref(), Some("the plan"));
    let waiting = drive_until(&mut world, |w| {
        live(w, &run_id).wait_reason == Some(WaitState::InteractionPoint)
    })
    .await;
    assert!(waiting, "the run says what it waits on");
    let state = live(&mut world, &run_id);
    assert_eq!(state.status, RunStatus::Waiting);
    assert_eq!(state.phase, PipelinePhase::AwaitingPerson);
    assert_eq!(state.interactions.len(), 1);

    assert!(starter.hub.answer(InteractionResponse::choice(asked.id, 0)));
    let finished = drive_until(&mut world, |w| {
        live(w, &run_id).status == RunStatus::Complete
    })
    .await;
    assert!(finished, "the approved run carries on to its end");
}

/// A blueprint that splits its task over workers that run its `work` stage,
/// then merges what they found.
const SPLITTER: &str = r#"[blueprint]
name = "splitter"
version = "0.0.0"
description = "Splits, works, merges."

[graph]
entry = "split"
inputs = [{ name = "task", type = { kind = "text", multiline = true }, binds = [{ region = "task" }] }]

[graph.layout]
total_budget_tokens = 50000
regions = [
    { name = "task", kind = "pinned", budget = 2000 },
    { name = "conversation", kind = { kind = "sliding_window", max_items = 40 }, budget = 20000 },
]

[[graph.stages]]
name = "split"
model = { models = [{ provider = "anthropic", model = "m" }] }
system_prompt = "Split."

[graph.stages.mode.fan_out]
worker = { stage = "work" }
merge_stage = "merge"
max_workers = 4

[[graph.stages]]
name = "work"
model = { models = [{ provider = "anthropic", model = "m" }] }
allow_as_worker = true
system_prompt = "Work."

[[graph.stages]]
name = "merge"
model = { models = [{ provider = "anthropic", model = "m" }] }
system_prompt = "Merge."

[[graph.edges]]
name = "merge"
from = "split"
to = "merge"
"#;

/// A fan-out whose workers finished before the daemon stopped comes back with
/// their results, read from their own run files, and keeps waiting on the
/// worker still running; none is counted as failed.
#[tokio::test]
async fn finished_fan_out_workers_come_back_as_done_after_a_restart() {
    use leviath_runtime::spec::graph::StageMode;
    use leviath_runtime::spec::names::RunId;
    use leviath_runtime::state::{FanOutState, FinalOutputState};
    let agent = tempfile::tempdir().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let manifest = manifest_in(agent.path(), SPLITTER);
    let parent = run_on_disk(Config::default(), registry(), runs.path(), &manifest);
    let finished = run_on_disk(Config::default(), registry(), runs.path(), &manifest);
    let failed = run_on_disk(Config::default(), registry(), runs.path(), &manifest);
    let running = run_on_disk(Config::default(), registry(), runs.path(), &manifest);
    change(runs.path(), &finished, |s| {
        s.status = RunStatus::Complete;
        s.phase = PipelinePhase::Done;
        s.final_output = Some(FinalOutputState {
            bytes: 11,
            format: None,
            stage: s.cursor.stage.clone(),
            submitted_at: 1,
            truncated: false,
            artifacts: Vec::new(),
        });
        s.files.final_output = Some(leviath_runtime::state::FileRef::whole(
            leviath_core::FINAL_OUTPUT_FILE,
            b"w1 found it",
        ));
    });
    std::fs::write(
        runs.path()
            .join(finished.as_str())
            .join(leviath_core::FINAL_OUTPUT_FILE),
        "w1 found it",
    )
    .unwrap();
    change(runs.path(), &failed, |s| {
        s.status = RunStatus::Error("w2 broke".to_string());
        s.phase = PipelinePhase::Done;
    });
    let config = {
        let reader =
            leviath_runtime::runfile::RunFileReader::open(&run_file(runs.path(), &parent)).unwrap();
        match &reader.spec().graph.stages[0].mode {
            StageMode::FanOut(def) => def.clone(),
            other => panic!("the split stage fans out: {other:?}"),
        }
    };
    let id = |run: &str| RunId::new(run).unwrap();
    change(runs.path(), &parent, |s| {
        s.status = RunStatus::Waiting;
        s.phase = PipelinePhase::FanOut;
        s.children = vec![id(&finished), id(&failed), id(&running)];
        s.wait_reason = Some(WaitState::FanOutWorkers(3));
        s.fan_out = Some(FanOutState {
            stage: s.cursor.stage.clone(),
            config,
            max_workers: Some(4),
            queued: Vec::new(),
            active: vec![
                ("w1".to_string(), id(&finished)),
                ("w2".to_string(), id(&failed)),
                ("w3".to_string(), id(&running)),
            ],
            done: Vec::new(),
            failed: Vec::new(),
            paused: false,
            origin: Default::default(),
            parts: Vec::new(),
        });
    });

    let starter = starter(Config::default(), registry(), runs.path());
    let mut world = world_for(&starter);
    resume_all(&mut world, &starter, runs.path());

    let fan_out = live(&mut world, &parent)
        .fan_out
        .expect("still fanning out");
    assert_eq!(
        fan_out.done,
        [("w1".to_string(), "w1 found it".to_string())]
    );
    assert_eq!(fan_out.failed, [("w2".to_string(), "w2 broke".to_string())]);
    assert_eq!(fan_out.active, [("w3".to_string(), id(&running))]);
}

/// After a daemon died, a call it had running comes back interrupted rather
/// than run again (the command may still be running), and a call that
/// finished keeps its result. After a clean stop the same call is run again.
/// A batch stopped on a question is asked again either way.
#[tokio::test]
async fn a_call_running_when_the_daemon_died_comes_back_interrupted() {
    use leviath_runtime::state::ToolResultState;
    let call = |id: &str, name: &str| ToolCallState {
        id: id.to_string(),
        name: name.to_string(),
        args: leviath_core::JsonDoc::new(serde_json::json!({"command": "sleep 15"})),
        thought_signature: None,
    };
    for crashed in [true, false] {
        let agent = tempfile::tempdir().unwrap();
        let runs = tempfile::tempdir().unwrap();
        let manifest = manifest_in(agent.path(), ASKER);
        let working = run_on_disk(Config::default(), registry(), runs.path(), &manifest);
        let asking = run_on_disk(Config::default(), registry(), runs.path(), &manifest);
        change(runs.path(), &working, |s| {
            s.phase = PipelinePhase::AwaitingTools;
            s.pending = Some(PendingBatch {
                calls: vec![call("c1", "shell"), call("c2", "shell")],
                done: [(
                    "c1".to_string(),
                    ToolResultState {
                        text: "ran c1".to_string(),
                        is_error: false,
                    },
                )]
                .into(),
            });
        });
        change(runs.path(), &asking, |s| {
            s.phase = PipelinePhase::AwaitingTools;
            s.pending = Some(PendingBatch {
                calls: vec![call("q", "ask_user_text"), call("c2", "shell")],
                done: Default::default(),
            });
        });
        if crashed {
            // What a daemon that died leaves behind.
            assert!(!leviath_runtime::restore::begin_session(runs.path()));
        }

        let starter = starter(Config::default(), registry(), runs.path());
        let mut world = world_for(&starter);
        resume_all(&mut world, &starter, runs.path());

        let done = live(&mut world, &working).pending.expect("in flight").done;
        assert_eq!(done["c1"].text, "ran c1");
        match crashed {
            true => {
                assert_eq!(
                    done["c2"].text,
                    leviath_runtime::restore::INTERRUPTED_TOOL_RESULT
                );
                assert!(done["c2"].is_error);
            }
            false => assert!(!done.contains_key("c2"), "a clean stop runs it again"),
        }
        let asked = live(&mut world, &asking).pending.expect("in flight").done;
        assert!(asked.is_empty(), "the question is asked again");
        // This daemon's own session is now the one marked as running.
        assert!(leviath_runtime::restore::begin_session(runs.path()));
    }
}

/// What a person approved for the run or its stage, and what it has written
/// against its ceilings, come back with a run the daemon restarts.
#[tokio::test]
async fn grants_and_writes_come_back_after_a_restart() {
    let agent = tempfile::tempdir().unwrap();
    let runs = tempfile::tempdir().unwrap();
    let manifest = manifest_in(agent.path(), ASKER);
    let run = run_on_disk(Config::default(), registry(), runs.path(), &manifest);
    change(runs.path(), &run, |s| {
        s.grants = leviath_runtime::state::Grants {
            run: vec!["cargo test".to_string()],
            stage: vec!["ls".to_string()],
            stage_index: Some(0),
            cleared: Vec::new(),
        };
        s.written = 4096;
    });

    let starter = starter(Config::default(), registry(), runs.path());
    let mut world = world_for(&starter);
    resume_all(&mut world, &starter, runs.path());

    let entity = entity_of(&mut world, &run);
    let grants = world
        .world()
        .get::<leviath_runtime::pipeline::ToolGrants>(entity)
        .expect("the run's grants");
    assert!(grants.granted("cargo test"));
    assert!(grants.granted("ls"));
    let state = live(&mut world, &run);
    assert_eq!(state.written, 4096);
    assert_eq!(state.grants.run, ["cargo test"]);
}
