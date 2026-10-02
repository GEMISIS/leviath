//! Converting real old run directories into run files.
//!
//! The fixtures are run directories written by Leviath 0.6.4: one real run
//! (`real-finished`) and five made against the mock provider (a finished
//! run, an errored one, a fan-out parent parked on its workers, one of those
//! workers parked at an interaction point, and a run whose daemon died with a
//! tool batch in flight). Personal paths in them were replaced with
//! `/home/user`.

mod common;
#[path = "convert/edges.rs"]
mod edges;
#[path = "convert/journal.rs"]
mod journal;
#[path = "convert/lookup.rs"]
mod lookup;

use common::{FIXTURES, Run, RunFile};
use leviath_legacy_runs::{BlueprintSource, ConvertError, is_legacy};
use leviath_runtime::spec::inputs::InputValue;
use leviath_runtime::spec::launch::Unattended;
use leviath_runtime::spec::run_spec::SpecOrigin;
use leviath_runtime::state::{PipelinePhase, RunEvent, RunStatus};

/// Every field no old run records, which every conversion must name.
const ALWAYS_DEFAULTED: &[&str] = &[
    "env",
    "launch.allow",
    "launch.max_depth",
    "launch.seed_commands",
    "launch.capture_model_input",
    "launch.regions",
    "launch.parts",
    "placement.worker_stage",
    "stages.*.tools",
    "stages.*.region_budgets",
    "progress",
    "totals.spend.reported_calls",
    "inbox",
    "last_transition",
    "context.entries.timestamp",
    "context.regions.needs_message_compaction",
];

#[test]
fn every_fixture_converts_to_a_run_file_that_folds_to_its_last_state() {
    for name in FIXTURES {
        let run = Run::fixture(name);
        assert!(is_legacy(&run.dir));
        let (report, file) = run.converted();
        assert!(!is_legacy(&run.dir));
        assert_eq!(report.run_file, run.path("run.lvr"));
        assert_eq!(file.states.len(), 2);
        assert_eq!(file.states[0].seq, 0);
        assert_eq!(file.fold(), file.last);
        assert_eq!(file.states[1], file.last);
        assert_eq!(file.last.seq, file.deltas.len() as u64);
        assert_eq!(report.deltas, file.deltas.len());
        assert_eq!(file.spec.run_id, report.run_id);
        assert_eq!(file.spec.env, Default::default());
        assert!(file.spec.launch.allow.is_empty());
        for field in ALWAYS_DEFAULTED {
            assert!(report.defaulted(field).is_some(), "{name}: {field}");
        }
        let logged: Vec<&RunEvent> = file.deltas.last().unwrap().events.iter().collect();
        for d in &report.defaulted {
            let line = format!("converted from the old layout: {d}");
            assert!(logged.contains(&&RunEvent::Log(line)));
        }
        for old in ["meta.json", "run.lvr", "context.json", "stages.json"] {
            assert!(report.legacy_dir.join(old).is_file(), "{name}: {old}");
        }
    }
}

#[test]
fn converting_twice_is_refused_and_changes_nothing() {
    let run = Run::fixture("finished");
    run.convert().unwrap();
    let before = std::fs::read(run.path("run.lvr")).unwrap();
    let err = run.convert().unwrap_err();
    assert!(
        matches!(&err, ConvertError::AlreadyConverted { path } if *path == run.path("run.lvr"))
    );
    assert!(err.to_string().contains("already a run file"));
    assert_eq!(std::fs::read(run.path("run.lvr")).unwrap(), before);
}

#[test]
fn a_finished_run_keeps_its_answer_and_its_blueprint_pin() {
    let run = Run::fixture("real-finished");
    let (report, file) = run.converted();
    assert_eq!(report.blueprint, BlueprintSource::Snapshot);
    assert_eq!(file.last.status, RunStatus::Complete);
    assert_eq!(file.last.phase, PipelinePhase::Done);
    let answer = file.last.final_output.as_ref().unwrap();
    assert!(answer.content.contains("atoms"));
    assert_eq!(answer.stage.as_str(), "engine");
    let SpecOrigin::Blueprint { blueprint, version } = &file.spec.origin else {
        panic!("a converted run comes from a blueprint");
    };
    assert_eq!(blueprint.name.as_str(), "McQueen");
    assert_eq!(version, "0.0.1");
    assert_eq!(
        blueprint.digest.as_ref().map(|d| d.as_str().len()),
        Some(64)
    );
    assert!(report.defaulted("origin.blueprint.digest").is_none());
    assert_eq!(file.spec.stages[0].provider.as_str(), "codex");
    assert_eq!(file.spec.stages[0].model.as_str(), "gpt-5.5");
    assert_eq!(
        file.spec.inputs.get("task"),
        Some(&InputValue::Text("tell me a joke".into()))
    );
    assert!(file.spec.seeded.contains_key("conversation"));
}

#[test]
fn a_finished_run_keeps_its_model_and_tool_calls_as_events() {
    let run = Run::fixture("finished");
    let (_, file) = run.converted();
    let events: Vec<&RunEvent> = file.deltas.iter().flat_map(|d| &d.events).collect();
    let inferences = events
        .iter()
        .filter(|e| matches!(e, RunEvent::Inference { .. }))
        .count();
    assert_eq!(inferences, 3);
    assert!(events.iter().any(|e| matches!(
        e,
        RunEvent::Inference { finish_reason: Some(r), .. } if r == "tool_call"
    )));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RunEvent::ToolStarted(c) if c.name == "current_time"))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RunEvent::ToolFinished { call_id, .. } if call_id == "call_1"))
    );
    assert!(file.deltas.iter().any(|d| {
        d.changes
            .iter()
            .any(|c| matches!(c, leviath_runtime::state::Change::Context(_)))
    }));
    assert_eq!(file.spec.launch.unattended, Unattended::All);
    assert_eq!(file.last.visits.get("main"), Some(&1));
    assert_eq!(file.last.ledger[0].visits.len(), 1);
}

#[test]
fn an_errored_run_keeps_why() {
    let (_, file) = Run::fixture("errored").converted();
    let RunStatus::Error(why) = &file.last.status else {
        panic!("the run errored");
    };
    assert!(why.contains("API key was rejected"));
    assert_eq!(file.spec.launch.unattended, Unattended::Off);
}

#[test]
fn a_fan_out_parent_keeps_its_queue_and_its_workers() {
    let (report, file) = Run::fixture("fanout-parent").converted();
    assert_eq!(file.last.phase, PipelinePhase::FanOut);
    assert_eq!(file.last.status, RunStatus::Waiting);
    let fan = file.last.fan_out.as_ref().unwrap();
    assert_eq!(fan.stage.as_str(), "main");
    assert_eq!(fan.max_workers, 1);
    assert_eq!(fan.queued[0].id, "beta");
    assert_eq!(
        fan.queued[0].inputs.get("task"),
        Some(&InputValue::Text(
            "Work item id: beta\nContext: {\"n\":2}".into()
        ))
    );
    assert_eq!(fan.active[0].0, "alpha");
    assert_eq!(file.last.children, vec![fan.active[0].1.clone()]);
    assert!(report.defaulted("fan_out.queued.*.inputs").is_some());
    assert!(report.notes.iter().any(|n| n.contains("origin")));
    assert_eq!(file.spec.launch.max_depth, 2);
    assert!(file.spec.seeded.contains_key("notes"));
}

#[test]
fn a_worker_at_an_interaction_point_keeps_the_question() {
    let (report, file) = Run::fixture("interaction-worker").converted();
    let BlueprintSource::Installed(path) = &report.blueprint else {
        panic!("the worker kept no blueprint copy");
    };
    assert!(path.ends_with("agents/waiter/agent.leviath"));
    assert_eq!(file.last.phase, PipelinePhase::AwaitingPerson);
    let open = &file.last.interactions[0];
    assert_eq!(open.prompt, "Approve the result?");
    assert_eq!(open.options, vec!["Approve", "Abort"]);
    assert!(report.defaulted("interactions[0].id").is_some());
    assert_eq!(
        file.spec.placement.parent.as_ref().map(|p| p.as_str()),
        Some("fanner-1790811839-97bde36542db")
    );
    assert_eq!(file.spec.placement.depth, 1);
}

#[test]
fn a_run_stopped_mid_tool_batch_keeps_the_batch_in_flight() {
    let (_, file) = Run::fixture("mid-tool-batch").converted();
    assert_eq!(file.last.status, RunStatus::Active);
    assert_eq!(file.last.phase, PipelinePhase::AwaitingTools);
    let batch = file.last.pending.as_ref().unwrap();
    assert_eq!(batch.calls[0].name, "shell");
    assert_eq!(
        batch.calls[0].args.value(),
        &serde_json::json!({"command": "sleep 120"})
    );
    assert!(batch.done.is_empty());
    assert_eq!(file.last.clock.since, None);
}

#[test]
fn the_file_reader_sees_only_the_frames_written() {
    let run = Run::fixture("finished");
    run.convert().unwrap();
    let file = RunFile::read(&run.path("run.lvr"));
    assert!(file.code.is_empty());
    assert!(file.blobs.is_empty());
}
