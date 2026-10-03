//! A converted run reads back the way the release that wrote it listed it:
//! the graph it ran even when its installed blueprint has changed since,
//! what `meta.json` says even when the journal says otherwise, and a run
//! nothing can answer any more ended rather than left waiting.

use leviath_core::run_meta::RunStatus as OldStatus;
use leviath_legacy_runs::{ConvertEnv, convert};
use leviath_runtime::runfile::RunFileReader;
use leviath_runtime::spec::run_spec::SpecOrigin;
use leviath_runtime::state::{PipelinePhase, RunStatus, StageFile};
use serde_json::json;

use crate::common::{Run, RunFile, fixtures_dir};

/// The run's record as `lev ps` reads it from the converted file.
fn listed(run: &Run) -> leviath_core::run_meta::RunMeta {
    let reader = RunFileReader::open(&run.path("run.lvr")).unwrap();
    leviath_runtime::runfile::summary(&reader).unwrap()
}

/// An agents directory whose `probe` is the one the fixtures ran, with a
/// stage `plan` put before its `main`: the blueprint gained a stage since
/// the run ran.
fn changed_agents() -> tempfile::TempDir {
    let agents = tempfile::tempdir().unwrap();
    let dir = agents.path().join("probe");
    std::fs::create_dir_all(&dir).unwrap();
    let text = std::fs::read_to_string(fixtures_dir().join("agents/probe/agent.leviath")).unwrap();
    let text = text.replace("entry_stage = \"main\"", "entry_stage = \"plan\"")
        + r#"
[stages.plan]
mode = "autonomous"
description = "Plan first"
model = { models = [{ provider = "openai", model = "gpt-mock" }] }
max_iterations = 2
system_prompt = "Plan."
"#;
    // `plan` is written last, so put it first: stage order is file order.
    let (head, main) = text.split_once("[stages.main]").unwrap();
    let (main, plan) = main.split_once("[stages.plan]").unwrap();
    std::fs::write(
        dir.join("agent.leviath"),
        format!("{head}[stages.plan]{plan}\n[stages.main]{main}"),
    )
    .unwrap();
    agents
}

/// Convert `run` against the agents in `agents`, as a run that kept no copy
/// of its blueprint is.
fn convert_against(run: &Run, agents: &tempfile::TempDir) -> RunFile {
    run.remove("blueprint.leviath");
    let env = ConvertEnv {
        agents_dir: Some(agents.path().to_path_buf()),
        stages: None,
    };
    convert(&run.dir, &env).unwrap();
    RunFile::read(&run.path("run.lvr"))
}

#[test]
fn a_finished_run_whose_blueprint_gained_a_stage_keeps_the_graph_it_ran() {
    let agents = changed_agents();
    let run = Run::fixture("finished");
    let file = convert_against(&run, &agents);
    let SpecOrigin::Recorded { why, .. } = &file.spec.origin else {
        panic!("{:?}", file.spec.origin);
    };
    assert!(
        why.contains("2 stages") && why.contains("recorded 1"),
        "{why}"
    );
    let stages: Vec<&str> = file
        .spec
        .graph
        .stages
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(stages, ["main"]);
    let meta = listed(&run);
    assert_eq!((meta.current_stage.as_str(), meta.stage_index), ("main", 0));
    assert_eq!(meta.num_stages, 1);
    assert_eq!(meta.status, OldStatus::Complete);
    // The stage's logs are still its own.
    let logs = file.last.files.stage_file(0, StageFile::Output).unwrap();
    assert_eq!(logs.path, "stages/0/output.log");
    assert_eq!(file.fold(), file.last);
}

#[test]
fn a_finished_run_whose_stages_were_reordered_keeps_the_order_it_ran() {
    let run = Run::fixture("finished");
    // The run recorded two stages, `main` then `plan`, and finished in
    // `plan`; the installed blueprint now has them the other way round.
    run.json("stages.json", |v| {
        let mut second = v[0].clone();
        second["name"] = json!("plan");
        v.as_array_mut().unwrap().push(second);
    });
    run.meta(|m| {
        m.num_stages = 2;
        m.current_stage = "plan".into();
        m.stage_index = 1;
    });
    let agents = changed_agents();
    let file = convert_against(&run, &agents);
    let SpecOrigin::Recorded { why, .. } = &file.spec.origin else {
        panic!("{:?}", file.spec.origin);
    };
    assert!(why.contains("in another order"), "{why}");
    let meta = listed(&run);
    assert_eq!((meta.current_stage.as_str(), meta.stage_index), ("plan", 1));
    assert_eq!(meta.num_stages, 2);
}

#[test]
fn a_run_whose_stage_moved_is_not_read_against_the_new_blueprint() {
    let run = Run::fixture("finished");
    // No ledger to compare, but the record says where its stage was.
    run.write("stages.json", "[]");
    run.meta(|m| m.num_stages = 0);
    let agents = changed_agents();
    let file = convert_against(&run, &agents);
    let SpecOrigin::Recorded { why, .. } = &file.spec.origin else {
        panic!("{:?}", file.spec.origin);
    };
    assert!(why.contains("stage 2") && why.contains("stage 1"), "{why}");
    assert_eq!(listed(&run).stage_index, 0);
}

#[test]
fn a_run_the_blueprint_still_matches_is_read_from_the_blueprint() {
    let run = Run::fixture("finished");
    run.remove("blueprint.leviath");
    let (_, file) = run.converted();
    assert!(
        matches!(file.spec.origin, SpecOrigin::Blueprint { .. }),
        "{:?}",
        file.spec.origin
    );
}

#[test]
fn an_unfinished_run_whose_blueprint_changed_ends_and_says_why() {
    let run = Run::fixture("finished");
    run.journal(|r| {
        r.retain(|r| {
            !matches!(
                r,
                leviath_legacy_runs::journal::JournalRecord::StatusChanged { .. }
            )
        })
    });
    run.meta(|m| m.status = OldStatus::Paused);
    let agents = changed_agents();
    let file = convert_against(&run, &agents);
    let RunStatus::Error(why) = &file.last.status else {
        panic!("{:?}", file.last.status);
    };
    assert!(why.contains("cannot resume"), "{why}");
    assert!(why.contains("2 stages"), "{why}");
    assert_eq!(file.last.phase, PipelinePhase::Done);
    assert_eq!(listed(&run).stage_index, 0);
}

/// `meta.json` was written after the journal's last record, and is what
/// every earlier release listed: a run cancelled during a model call.
#[test]
fn a_finished_run_lists_what_its_meta_json_says() {
    let run = Run::fixture("finished");
    run.json("meta.json", |v| {
        v["status"] = json!("cancelled");
        v["title"] = json!("done");
        v["title_error"] = json!("the title model failed");
        v["prompt_tokens"] = json!(1200);
        v["completion_tokens"] = json!(300);
        v["cost_usd"] = json!(null);
        v["cost_is_exact"] = json!(false);
        v["tool_calls"] = json!(7);
    });
    let (_, file) = run.converted();
    let meta = listed(&run);
    assert_eq!(meta.status, OldStatus::Cancelled);
    assert_eq!(meta.title.as_deref(), Some("done"));
    assert_eq!(meta.title_error.as_deref(), Some("the title model failed"));
    assert_eq!((meta.prompt_tokens, meta.completion_tokens), (1200, 300));
    assert_eq!(meta.cost_usd, None);
    assert!(!meta.cost_is_exact);
    assert_eq!(meta.tool_calls, 7);
    assert_eq!(file.fold(), file.last);
}

/// A run 0.6.4 brought back at start was restored from its journal: its
/// stage, iteration and totals, which `meta.json` can trail by a step.
#[test]
fn a_run_brought_back_at_start_keeps_its_journal_position() {
    let run = Run::fixture("finished");
    run.journal(|r| {
        r.retain(|r| {
            !matches!(
                r,
                leviath_legacy_runs::journal::JournalRecord::StatusChanged { .. }
            )
        })
    });
    run.meta(|m| m.status = OldStatus::Paused);
    run.json("meta.json", |v| {
        v["prompt_tokens"] = json!(1);
        v["iteration"] = json!(1);
        v["title"] = json!("kept");
    });
    let journal = run
        .records()
        .iter()
        .rev()
        .find_map(|r| match r {
            leviath_legacy_runs::journal::JournalRecord::Progress { meta, .. } => {
                Some((**meta).clone())
            }
            _ => None,
        })
        .unwrap();
    let (_, file) = run.converted();
    let meta = listed(&run);
    assert_eq!(meta.status, OldStatus::Paused);
    assert_eq!(meta.prompt_tokens, journal.prompt_tokens);
    assert_eq!(file.last.cursor.iteration as usize, journal.iteration);
    assert_eq!(meta.title.as_deref(), Some("kept"));
}

/// Leviath 0.1.0 kept a question to a person in `pending.json`, answered by
/// a worker process that is gone, with no record of the call that asked it.
#[test]
fn a_run_waiting_on_a_0_1_0_question_ends_and_says_why() {
    let run = Run::fixture("finished");
    // A run from before the journal: `meta.json` and `stages.json` only.
    run.remove("run.lvr");
    run.remove("context.json");
    run.json("meta.json", |v| v["status"] = json!("waiting_input"));
    run.write(
        "pending.json",
        &json!({
            "id": "1791014753-0",
            "kind": "free_text",
            "prompt": "What colour?",
            "options": [],
            "tool_name": null,
            "tool_arguments": null,
            "required": true,
            "stage_name": "main"
        })
        .to_string(),
    );
    let (report, file) = run.converted();
    let RunStatus::Error(why) = &file.last.status else {
        panic!("{:?}", file.last.status);
    };
    assert!(why.contains("What colour?"), "{why}");
    assert_eq!(file.last.phase, PipelinePhase::Done);
    assert!(file.last.interactions.is_empty());
    assert_eq!(listed(&run).status, OldStatus::Error);
    assert!(report.legacy_dir.join("pending.json").is_file());
    assert_eq!(file.fold(), file.last);
}

#[test]
fn a_0_1_0_question_that_does_not_read_is_still_named() {
    let run = Run::fixture("finished");
    run.json("meta.json", |v| v["status"] = json!("waiting_input"));
    run.write("pending.json", "{ not json");
    let (_, file) = run.converted();
    let RunStatus::Error(why) = &file.last.status else {
        panic!("{:?}", file.last.status);
    };
    assert!(why.contains("pending.json"), "{why}");
}

/// A finished run whose `pending.json` was left behind is listed as it was.
#[test]
fn a_finished_run_with_a_stale_question_file_is_left_finished() {
    let run = Run::fixture("finished");
    run.write("pending.json", "{}");
    let (_, file) = run.converted();
    assert_eq!(file.last.status, RunStatus::Complete);
}

/// The files a run uploaded to its providers are listed where a run in the
/// new layout keeps that list, so they are still forgotten when it ends.
#[test]
fn the_provider_upload_list_stays_where_it_is() {
    let run = Run::fixture("finished");
    run.write("provider-files.json", "{\"files\":[]}");
    let (report, _) = run.converted();
    assert!(run.path("provider-files.json").is_file());
    assert!(!report.legacy_dir.join("provider-files.json").exists());
}
