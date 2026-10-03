//! Old runs that every earlier release listed but whose files a straight
//! conversion could not read: a run whose blueprint is gone, an answer that
//! named its artifacts' type `media_type`, a record from before the run clock
//! and before the run flags. Each converts, and reads back the way 0.6.4
//! listed it.

use leviath_core::run_meta::RunStatus as OldStatus;
use leviath_legacy_runs::BlueprintSource;
use leviath_legacy_runs::journal::JournalRecord;
use leviath_runtime::runfile::RunFileReader;
use leviath_runtime::spec::inputs::InputValue;
use leviath_runtime::spec::run_spec::SpecOrigin;
use leviath_runtime::state::{PipelinePhase, RunStatus};
use serde_json::json;

use crate::common::Run;

/// The run's record as `lev ps` reads it from the converted file.
fn listed(run: &Run) -> leviath_core::run_meta::RunMeta {
    let reader = RunFileReader::open(&run.path("run.lvr")).unwrap();
    leviath_runtime::runfile::summary(&reader).unwrap()
}

/// Take the run's blueprint away: no snapshot, and nothing installed under
/// its name.
fn orphan(run: &Run) {
    run.remove("blueprint.leviath");
    run.meta(|m| {
        m.agent_name = "gone".into();
        m.agent_path = "/home/user/.leviath/agents/gone/agent.leviath".into();
    });
}

#[test]
fn a_finished_run_whose_blueprint_is_gone_converts_from_what_it_recorded() {
    let run = Run::fixture("finished");
    orphan(&run);
    let (report, file) = run.converted();
    assert!(
        matches!(&report.blueprint, BlueprintSource::Recorded { tried, .. } if !tried.is_empty()),
        "{:?}",
        report.blueprint
    );
    let SpecOrigin::Recorded {
        name,
        manifest,
        why,
    } = &file.spec.origin
    else {
        panic!("{:?}", file.spec.origin);
    };
    assert_eq!(name.as_str(), "gone");
    assert_eq!(manifest, "/home/user/.leviath/agents/gone/agent.leviath");
    assert!(why.contains("not in the run and not installed"), "{why}");
    // The stages it ran, on the models it ran them on.
    let stages: Vec<&str> = file
        .spec
        .graph
        .stages
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(stages, ["main"]);
    assert_eq!(file.spec.stages[0].provider.as_str(), "openai");
    assert_eq!(file.spec.stages[0].model.as_str(), "gpt-mock");
    assert!(file.spec.stages[0].tools.is_empty());
    // Its regions, so its context reads back.
    let regions: Vec<&str> = file
        .spec
        .graph
        .layout
        .regions
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert!(
        regions.contains(&"task") && regions.contains(&"conversation"),
        "{regions:?}"
    );
    assert_eq!(
        file.spec.inputs.get("task"),
        Some(&InputValue::Text("What time is it?".into()))
    );
    assert_eq!(file.last.status, RunStatus::Complete);
    assert_eq!(file.fold(), file.last);
    let meta = listed(&run);
    assert_eq!(meta.agent_name, "gone");
    assert_eq!(
        meta.agent_path,
        "/home/user/.leviath/agents/gone/agent.leviath"
    );
    assert_eq!(meta.task, "What time is it?");
    assert_eq!(meta.status, OldStatus::Complete);
    assert_eq!(meta.current_stage, "main");
}

#[test]
fn a_recorded_graph_has_every_stage_the_run_entered_and_the_edges_it_took() {
    let run = Run::fixture("finished");
    orphan(&run);
    run.json("stages.json", |v| {
        let mut second = v[0].clone();
        second["name"] = json!("review");
        second["models"] = json!([{ "provider": "anthropic", "model": "claude-x" }]);
        v.as_array_mut().unwrap().push(second);
    });
    // The journal saw it move from `main` to `review` and finish there.
    run.journal(|records| {
        let mut moved = records
            .iter()
            .find_map(|r| match r {
                JournalRecord::Header { meta, .. } => Some((**meta).clone()),
                _ => None,
            })
            .unwrap();
        moved.current_stage = "review".into();
        let ctx = records
            .iter()
            .find_map(|r| match r {
                JournalRecord::ContextCheckpoint { snapshot, .. } => Some(snapshot.clone()),
                _ => None,
            })
            .unwrap();
        records.push(JournalRecord::Checkpoint {
            meta: Box::new(moved),
            context: ctx,
            at: 1_790_811_836,
        });
    });
    run.json("meta.json", |v| v["current_stage"] = json!("review"));
    let (_, file) = run.converted();
    let stages: Vec<&str> = file
        .spec
        .graph
        .stages
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(stages, ["main", "review"]);
    let edges: Vec<(&str, &str)> = file
        .spec
        .graph
        .edges
        .iter()
        .map(|e| (e.from.as_str(), e.to.as_str()))
        .collect();
    assert_eq!(edges, [("main", "review")]);
    assert_eq!(file.spec.stages[1].provider.as_str(), "anthropic");
    assert_eq!(file.last.cursor.stage.as_str(), "review");
}

#[test]
fn an_unfinished_run_whose_blueprint_is_gone_converts_and_says_it_cannot_resume() {
    let run = Run::fixture("finished");
    orphan(&run);
    run.journal(|r| r.retain(|r| !matches!(r, JournalRecord::StatusChanged { .. })));
    run.meta(|m| m.status = OldStatus::Paused);
    let (report, file) = run.converted();
    let RunStatus::Error(why) = &file.last.status else {
        panic!("{:?}", file.last.status);
    };
    assert!(why.contains("cannot resume"), "{why}");
    assert_eq!(file.last.phase, PipelinePhase::Done);
    assert!(
        report.notes.iter().any(|n| n.contains("was paused")),
        "{:?}",
        report.notes
    );
}

#[test]
fn an_answer_whose_artifacts_name_a_media_type_converts() {
    let run = Run::fixture("real-finished");
    let artifact = json!({
        "name": "chart",
        "path": "chart.png",
        "media_type": "image/png",
        "size": 10402,
        "sha256": "375e1a786cbfd2682111e9476ac99c9d92a6334a244b1a6f9eace3df2b99bfa1"
    });
    run.json("meta.json", |v| {
        v["final_output"]["artifacts"] = json!([artifact])
    });
    // The journal's records carry the same answer, written the same way.
    let mut out = leviath_legacy_runs::journal::MAGIC.to_vec();
    out.extend(1u16.to_be_bytes());
    for r in run.records() {
        let mut v = serde_json::to_value(&r).unwrap();
        for record in v.as_object_mut().unwrap().values_mut() {
            if let Some(answer) = record
                .get_mut("meta")
                .and_then(|m| m.get_mut("final_output"))
                && answer.is_object()
            {
                answer["artifacts"] = json!([artifact]);
            }
        }
        let payload = serde_json::to_vec(&v).unwrap();
        out.extend((payload.len() as u64).to_be_bytes());
        out.extend(payload);
    }
    std::fs::write(run.path("run.lvr"), out).unwrap();
    assert!(
        String::from_utf8_lossy(&std::fs::read(run.path("run.lvr")).unwrap())
            .contains("media_type")
    );
    let (_, file) = run.converted();
    let answer = file.last.final_output.as_ref().unwrap();
    assert_eq!(answer.artifacts[0].name, "chart");
    assert_eq!(answer.artifacts[0].mime_type, "image/png");
}

#[test]
fn a_record_from_before_the_run_clock_works_for_its_wall_clock_span() {
    let run = Run::fixture("real-finished");
    run.meta(|m| {
        m.active = None;
        m.started_at = 1_000;
        m.updated_at = 1_600;
    });
    run.json("stages.json", |v| {
        for stage in v.as_array_mut().unwrap() {
            stage.as_object_mut().unwrap().remove("active");
            stage["started_at"] = json!(1_000);
            stage["ended_at"] = json!(1_450);
        }
    });
    // `meta.json` was written after the journal's last record, and is what
    // every earlier release listed.
    run.json("meta.json", |v| v["updated_at"] = json!(1_700));
    let (_, file) = run.converted();
    assert_eq!(file.last.clock.banked_secs, 700);
    assert_eq!(listed(&run).active_runtime_secs(9_000), 700);
    assert_eq!(file.last.ledger[0].clock.banked_secs, 450);
    assert_eq!(file.fold(), file.last);
}

#[test]
fn a_record_from_before_the_run_flags_is_not_called_empty() {
    let run = Run::fixture("finished");
    let strip = |v: &mut serde_json::Value| {
        v.as_object_mut().unwrap().remove("flags");
    };
    run.json("meta.json", strip);
    run.journal(|records| {
        for r in records.iter_mut() {
            if let JournalRecord::Header { meta, .. }
            | JournalRecord::Progress { meta, .. }
            | JournalRecord::Checkpoint { meta, .. } = r
            {
                meta.flags = Default::default();
            }
        }
        // A whole checkpoint of the record, as the journal wrote now and then.
        let JournalRecord::Header { meta, .. } = records[0].clone() else {
            panic!("the journal starts with its header");
        };
        let context = records
            .iter()
            .find_map(|r| match r {
                JournalRecord::ContextCheckpoint { snapshot, .. } => Some(snapshot.clone()),
                _ => None,
            })
            .unwrap();
        records.push(JournalRecord::Checkpoint {
            meta,
            context,
            at: 1_790_811_836,
        });
    });
    let (report, _) = run.converted();
    assert!(!listed(&run).flags.empty_output);
    assert!(report.defaulted("flags.no_output_tools").is_some());
    // A record that kept its flags is judged by them.
    let kept = Run::fixture("finished");
    kept.converted();
    assert!(listed(&kept).flags.empty_output);
    // One from before runs recorded whether they could change a file is
    // empty only when it said so itself.
    // A record that kept the flag but stopped before its verdict was written
    // says not empty as well.
    for (said, shown, keep) in [
        (false, false, false),
        (true, true, false),
        (false, false, true),
    ] {
        let older = Run::fixture("finished");
        older.json("meta.json", |v| {
            let flags = v["flags"].as_object_mut().unwrap();
            if !keep {
                flags.remove("no_output_tools");
            }
            flags.insert("empty_output".into(), json!(said));
        });
        let (report, _) = older.converted();
        assert_eq!(listed(&older).flags.empty_output, shown, "said {said}");
        assert_eq!(report.defaulted("flags.no_output_tools").is_some(), !said);
    }
}

#[test]
fn a_converted_run_lists_the_blueprint_file_it_ran() {
    let run = Run::fixture("real-finished");
    let (_, file) = run.converted();
    let SpecOrigin::Blueprint { manifest, .. } = &file.spec.origin else {
        panic!("{:?}", file.spec.origin);
    };
    assert_eq!(manifest, "/home/user/.leviath/agents/McQueen/agent.leviath");
    assert_eq!(
        listed(&run).agent_path,
        "/home/user/.leviath/agents/McQueen/agent.leviath"
    );
}

#[test]
fn every_blueprint_that_does_not_read_leaves_the_run_its_recorded_graph() {
    type BreakIt = fn(&Run);
    let cases: Vec<(&str, BreakIt, &str)> = vec![
        (
            "no blueprint",
            |r| {
                r.remove("blueprint.leviath");
                r.meta(|m| m.agent_name = "nobody".into());
            },
            "not in the run and not installed",
        ),
        (
            "bad blueprint",
            |r| r.write("blueprint.leviath", "[agent"),
            "does not parse",
        ),
        (
            "bad graph",
            |r| {
                let text = std::fs::read_to_string(r.path("blueprint.leviath")).unwrap();
                r.write(
                    "blueprint.leviath",
                    &text.replace("\"shell\"", "\"bad tool\""),
                );
            },
            "not a valid run graph",
        ),
        (
            "bad server table",
            |r| {
                let text = std::fs::read_to_string(r.path("blueprint.leviath")).unwrap();
                r.write(
                    "blueprint.leviath",
                    &format!("{text}\n[[mcp_servers]]\nname = \"bad name\"\n"),
                );
            },
            "not a valid run graph",
        ),
    ];
    for (what, break_it, says) in cases {
        let run = Run::fixture("finished");
        break_it(&run);
        let (_, file) = run.converted();
        let SpecOrigin::Recorded { why, .. } = &file.spec.origin else {
            panic!("{what}: {:?}", file.spec.origin);
        };
        assert!(why.contains(says), "{what}: {why}");
        assert_eq!(file.last.status, RunStatus::Complete, "{what}");
    }
}

#[test]
fn a_recorded_graph_keeps_each_region_as_its_snapshot_shows_it() {
    let run = Run::fixture("finished");
    orphan(&run);
    // No ledger, and no stage named anywhere: one stage, on the model the
    // run was launched with.
    run.write("stages.json", "[]");
    run.meta(|m| {
        m.current_stage = String::new();
        m.task = String::new();
    });
    let kinds = [
        ("pinned", "Pinned"),
        ("temporary", "Temporary"),
        ("clearable", "Clearable"),
        ("compacting", "Compacting"),
        ("compact_history", "CompactHistory"),
        ("history", "CompactHistory"),
        ("sliding_window", "SlidingWindow"),
        ("sliding", "SlidingWindow"),
        ("keyed", "Keyed"),
        ("checklist", "Checklist"),
        ("custom", "Pinned"),
    ];
    run.journal(|records| {
        for r in records.iter_mut() {
            if let JournalRecord::ContextCheckpoint { snapshot, .. } = r {
                let template = snapshot.regions[0].clone();
                snapshot.regions = kinds
                    .iter()
                    .map(|(k, _)| {
                        let mut region = template.clone();
                        region.name = format!("r-{k}");
                        region.kind = (*k).to_string();
                        region
                    })
                    .collect();
                let mut bad = template.clone();
                bad.name = String::new();
                snapshot.regions.push(bad);
            }
        }
    });
    let (_, file) = run.converted();
    let stages: Vec<&str> = file
        .spec
        .graph
        .stages
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(stages, ["stage"]);
    assert_eq!(listed(&run).task, "");
    assert_eq!(
        file.spec.graph.stages[0].model.models[0].to_string(),
        "openai/gpt-mock"
    );
    let shown: Vec<String> = file
        .spec
        .graph
        .layout
        .regions
        .iter()
        .map(|r| format!("{}={:?}", r.name, r.kind))
        .collect();
    for (k, want) in kinds {
        assert!(
            shown
                .iter()
                .any(|s| s.starts_with(&format!("r-{k}={want}"))),
            "{k}: {shown:?}"
        );
    }
}

/// The installed blueprint of a run that kept no copy can have changed since
/// it ran: one without a stage the run ran is not the one it ran, and the run
/// is read from what it recorded.
#[test]
fn a_blueprint_without_a_stage_the_run_ran_is_not_the_one_it_ran() {
    let run = Run::fixture("finished");
    run.meta(|m| m.current_stage = "gone".into());
    let (report, file) = run.converted();
    let SpecOrigin::Recorded { why, .. } = &file.spec.origin else {
        panic!("{:?}", file.spec.origin);
    };
    assert!(why.contains("has no stage \"gone\""), "{why}");
    assert_eq!(file.last.cursor.stage.as_str(), "gone");
    assert!(report.defaulted("cursor.stage").is_none());
}

/// A finished run keeps the models it ran on: nothing about this machine is
/// looked up for a run that never runs again.
#[test]
fn a_finished_run_is_not_looked_up_on_this_machine() {
    use leviath_legacy_runs::{ConvertEnv, StageLookup};
    struct Refuses;
    impl StageLookup for Refuses {
        fn model(
            &self,
            _: &leviath_runtime::spec::graph::RunGraph,
            _: &leviath_runtime::spec::graph::StageDef,
            _: Option<&leviath_runtime::spec::names::ModelRef>,
        ) -> Result<leviath_runtime::spec::env::ModelPlan, String> {
            panic!("a finished run is not looked up")
        }
        fn tools(
            &self,
            _: &leviath_runtime::spec::graph::RunGraph,
            _: &leviath_runtime::spec::graph::StageDef,
            _: &leviath_runtime::spec::env::CodeFiles,
            _: Option<&std::path::Path>,
            _: Option<&std::path::Path>,
        ) -> Result<leviath_runtime::spec::env::StageTools, String> {
            panic!("a finished run is not looked up")
        }
        fn default_max_depth(&self, _: &leviath_runtime::spec::graph::RunGraph) -> u8 {
            3
        }
    }
    let run = Run::fixture("finished");
    let env = ConvertEnv {
        agents_dir: crate::common::env().agents_dir,
        stages: Some(&Refuses),
    };
    let report = leviath_legacy_runs::convert(&run.dir, &env).unwrap();
    assert!(
        report.notes.iter().any(|n| n.contains("never runs again")),
        "{:?}",
        report.notes
    );
}

/// A record that names no cost shows none, rather than a cost of nothing.
#[test]
fn a_record_without_a_cost_shows_none() {
    let run = Run::fixture("real-finished");
    run.meta(|m| m.cost_usd = None);
    run.json("stages.json", |v| {
        for stage in v.as_array_mut().unwrap() {
            stage.as_object_mut().unwrap().remove("cost_usd");
        }
    });
    let (_, file) = run.converted();
    assert_eq!(listed(&run).cost_usd, None);
    assert!(file.last.ledger[0].spend.unpriced_calls >= 1);
    let kept = Run::fixture("real-finished");
    kept.converted();
    assert_eq!(listed(&kept).cost_usd, Some(0.0));
}

/// A run lists the task and the model it recorded, whatever its blueprint
/// now says: an old task region could carry its stage's instructions, and
/// the installed blueprint may have changed models since.
#[test]
fn a_run_lists_the_task_and_model_it_recorded() {
    let run = Run::fixture("finished");
    run.json("stages.json", |v| {
        for stage in v.as_array_mut().unwrap() {
            stage.as_object_mut().unwrap().remove("models");
        }
    });
    run.meta(|m| m.model = Some("anthropic/claude-x".into()));
    run.journal(|records| {
        for r in records.iter_mut() {
            if let JournalRecord::ContextCheckpoint { snapshot, .. } = r
                && let Some(task) = snapshot.regions.iter_mut().find(|r| r.name == "task")
            {
                for e in &mut task.entries {
                    e.content =
                        format!("{}\n[Stage instructions: be a probe]", e.content.as_str()).into();
                }
            }
        }
    });
    let (_, file) = run.converted();
    assert_eq!(file.spec.stages[0].provider.as_str(), "anthropic");
    assert_eq!(file.spec.stages[0].model.as_str(), "claude-x");
    let meta = listed(&run);
    assert_eq!(meta.task, "What time is it?");
    assert_eq!(meta.model.as_deref(), Some("anthropic/claude-x"));
}
