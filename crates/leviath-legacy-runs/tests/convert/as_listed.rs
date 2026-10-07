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

/// A graph the run recorded says why it was used rather than the blueprint:
/// here the blueprint read, and was not the graph the run ran.
#[test]
fn a_recorded_graph_says_why_it_was_used() {
    let agents = changed_agents();
    let run = Run::fixture("finished");
    let (report, file) = {
        run.remove("blueprint.leviath");
        let env = ConvertEnv {
            agents_dir: Some(agents.path().to_path_buf()),
            stages: None,
        };
        let report = convert(&run.dir, &env).unwrap();
        (report, RunFile::read(&run.path("run.lvr")))
    };
    let SpecOrigin::Recorded { why, .. } = &file.spec.origin else {
        panic!("{:?}", file.spec.origin);
    };
    let description = file.spec.graph.description.clone().unwrap_or_default();
    assert!(description.contains(why.as_str()), "{description}");
    assert!(!description.contains("could not be read"), "{description}");
    let said: Vec<&String> = report
        .notes
        .iter()
        .chain(report.defaulted.iter().map(|d| &d.why))
        .collect();
    assert!(
        !said
            .iter()
            .any(|n| n.contains("blueprint the run ran could not be read")),
        "{said:?}"
    );
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
    // It is never brought back, so it lists the manifest its record names.
    assert_eq!(
        listed(&run).agent_path,
        "/home/user/lve/.leviath/agents/probe/agent.leviath"
    );
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

/// What a run recorded about itself lists as its record said, wherever this
/// build would list the same run otherwise: the model it named (or none),
/// its stages (none for a run refused before it started), its blueprint's
/// revision (or none) and the depth cap of its tree of child runs (none
/// until it started one). How it was doing lists as its record said while
/// it stands where it was converted: no progress stamp and no working clock
/// where it kept none, and not empty where it said so.
#[test]
fn a_run_lists_what_it_recorded_about_itself() {
    let run = Run::fixture("finished");
    run.json("meta.json", |v| {
        v["model"] = json!(null);
        v["num_stages"] = json!(0);
        v["max_child_depth"] = json!(0);
        let m = v.as_object_mut().unwrap();
        m.remove("blueprint_digest");
        m.remove("last_progress_at");
        m.remove("active");
    });
    let (_, file) = run.converted();
    assert_eq!(file.spec.graph.stages.len(), 1);
    let meta = listed(&run);
    assert_eq!(meta.model, None);
    assert_eq!(meta.num_stages, 0);
    assert_eq!(meta.max_child_depth, 0);
    assert_eq!(meta.blueprint_digest, None);
    assert_eq!(meta.last_progress_at, None);
    assert_eq!(meta.active, None);
    assert!(meta.flags.empty_output);

    // Once the run moves on, how it is doing lists as this build lists any
    // run; what it recorded about itself does not change.
    let tail = leviath_runtime::runfile::RunFileTail::read(&run.path("run.lvr")).unwrap();
    let mut moved = tail.state.clone();
    moved.seq += 1;
    moved.flags.modified_file_count = 1;
    let later = leviath_runtime::runfile::summary_of(&tail.spec, &moved, tail.updated_at + 5);
    assert_eq!(later.last_progress_at, Some(tail.updated_at + 5));
    assert!(later.active.is_some());
    assert!(!later.flags.empty_output);
    assert_eq!((later.model, later.num_stages), (None, 0));

    // A child run lists the depth cap it recorded, not what is left of it.
    let child = Run::fixture("finished");
    child.meta(|m| {
        m.depth = 1;
        m.max_child_depth = 2;
        m.parent_run_id = Some("probe-1790811800-aaaaaaaaaaaa".into());
    });
    let (_, file) = child.converted();
    assert_eq!(file.spec.launch.max_depth, 1);
    let meta = listed(&child);
    assert_eq!(meta.max_child_depth, 2);
    assert_eq!(
        meta.blueprint_digest.as_deref(),
        Some("fec796f35c707cada2ccaddd6150af491f892a1c28966004c8344d632a3316a5")
    );
    assert_eq!(meta.model.as_deref(), Some("openai/gpt-mock"));
    assert_eq!(meta.last_progress_at, Some(1_790_811_836));
    assert_eq!(meta.active.map(|a| a.banked_secs), Some(0));
}

/// A ledger from a release that did not count visits lists none, and the
/// run still names the stage it is in.
#[test]
fn a_ledger_that_did_not_count_visits_lists_none() {
    let run = Run::fixture("finished");
    run.json("stages.json", |v| {
        for stage in v.as_array_mut().unwrap() {
            stage["visit_count"] = json!(0);
            stage["visits"] = json!([]);
        }
    });
    let (_, file) = run.converted();
    assert!(file.last.visits.is_empty(), "{:?}", file.last.visits);
    let stages = leviath_runtime::runfile::stage_records(&file.spec, &file.last);
    assert_eq!((stages[0].visit_count, stages[0].entered), (0, true));
    assert_eq!(listed(&run).current_stage, "main");
    assert_eq!(file.fold(), file.last);
}

/// A converted run's stage ledger lists whether each stage was entered and
/// its working clocks as its record kept them while it stands where it was
/// converted: a zero clock as a zero clock, none where the record kept none,
/// and not entered where a release that did not keep the answer said so.
/// The state still knows the stage ran, so its status reads right, and once
/// the run moves on the ledger lists as any run's does.
#[test]
fn a_converted_ledger_lists_entered_and_clocks_as_recorded() {
    let run = Run::fixture("finished");
    run.json("stages.json", |v| {
        let stage = &mut v.as_array_mut().unwrap()[0];
        let m = stage.as_object_mut().unwrap();
        m.remove("entered");
        m.remove("active");
    });
    let (_, file) = run.converted();
    assert!(file.last.ledger[0].entered, "the state knows it ran");
    let stages = leviath_runtime::runfile::stage_records(&file.spec, &file.last);
    let clock =
        |a: Option<leviath_core::run_meta::ActiveClock>| a.map(|a| (a.banked_secs, a.since));
    assert!(!stages[0].entered);
    assert_eq!(clock(stages[0].active), None);
    assert_eq!(clock(stages[0].visits[0].active), Some((0, None)));
    let body = serde_json::to_value(&stages[0]).unwrap();
    assert_eq!(body["active"], json!(null));
    assert_eq!(
        body["visits"][0]["active"],
        json!({"banked_secs": 0, "since": null})
    );

    let mut moved = file.last.clone();
    moved.seq += 1;
    let later = leviath_runtime::runfile::stage_records(&file.spec, &moved);
    assert!(later[0].entered);
    assert!(later[0].active.is_some());
}

/// A converted run's history starts when its journal first held the window,
/// as every earlier release listed it, and a run that kept no journal, which
/// those releases listed no history for, starts none.
#[test]
fn a_converted_history_starts_when_its_journal_first_held_the_window() {
    use leviath_legacy_runs::journal::JournalRecord;
    let run = Run::fixture("finished");
    let first = run
        .records()
        .iter()
        .find_map(|r| match r {
            JournalRecord::ContextCheckpoint { at, .. }
            | JournalRecord::ContextDiff { at, .. }
            | JournalRecord::Progress { at, .. }
            | JournalRecord::Checkpoint { at, .. } => Some(*at),
            _ => None,
        })
        .expect("the fixture's journal holds a window");
    let (_, file) = run.converted();
    assert_eq!(file.spec.listed.unwrap().first_point_at, Some(first));

    let bare = Run::fixture("finished");
    bare.remove("run.lvr");
    let (_, file) = bare.converted();
    assert_eq!(file.spec.listed.unwrap().first_point_at, None);
}

/// A run that was not finished lists the copy of its blueprint it was
/// brought back from, as the release that wrote it listed it while it was:
/// converted, that copy is under `legacy/`. One that kept no copy, whose
/// record names a manifest that is gone, lists the `agent.toml` its agent
/// was upgraded to. A finished run lists the manifest its record names.
#[test]
fn an_unfinished_run_lists_a_blueprint_that_is_there() {
    let paused = |run: &Run| {
        run.journal(|r| {
            r.retain(|r| {
                !matches!(
                    r,
                    leviath_legacy_runs::journal::JournalRecord::StatusChanged { .. }
                )
            })
        });
        run.meta(|m| m.status = OldStatus::Paused);
    };
    let run = Run::fixture("finished");
    paused(&run);
    run.converted();
    let kept = run.path("legacy").join("blueprint.leviath");
    assert!(kept.is_file());
    assert_eq!(listed(&run).agent_path, kept.to_string_lossy());

    let done = Run::fixture("finished");
    done.converted();
    assert_eq!(
        listed(&done).agent_path,
        "/home/user/lve/.leviath/agents/probe/agent.leviath"
    );

    // No copy: the installed agent was upgraded to an `agent.toml`.
    let agents = tempfile::tempdir().unwrap();
    let dir = agents.path().join("probe");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(
        fixtures_dir().join("agents/probe/agent.leviath"),
        dir.join("agent.leviath"),
    )
    .unwrap();
    std::fs::write(dir.join("agent.toml"), "# upgraded\n").unwrap();
    let env = ConvertEnv {
        agents_dir: Some(agents.path().to_path_buf()),
        stages: None,
    };
    let upgraded = Run::fixture("finished");
    paused(&upgraded);
    upgraded.remove("blueprint.leviath");
    convert(&upgraded.dir, &env).unwrap();
    assert_eq!(
        listed(&upgraded).agent_path,
        dir.join("agent.toml").to_string_lossy()
    );

    // No copy, and the manifest the record names is still there.
    let named = Run::fixture("finished");
    paused(&named);
    named.remove("blueprint.leviath");
    let manifest = dir.join("agent.leviath").to_string_lossy().into_owned();
    named.meta(|m| m.agent_path = manifest.clone());
    convert(&named.dir, &env).unwrap();
    assert_eq!(listed(&named).agent_path, manifest);
}
