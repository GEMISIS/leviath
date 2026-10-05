//! Old run directories that are broken, partial or unusual.

use leviath_core::run_meta::{RunStatus as OldStatus, WaitReason};
use leviath_legacy_runs::journal::JournalRecord;
use leviath_legacy_runs::{BlueprintSource, ConvertEnv, ConvertError, convert};
use leviath_runtime::spec::inputs::InputValue;
use leviath_runtime::spec::launch::Unattended;
use leviath_runtime::spec::names::ProfileName;
use leviath_runtime::state::context::PartBody;
use leviath_runtime::state::{EntryKind, EntryMeta, PipelinePhase, RunStatus, StageStatus};
use serde_json::json;

use crate::common::Run;

/// A two-stage blueprint with hooks, caps, taint tracking and two caller
/// inputs besides the task.
const RICH: &str = r#"
[agent]
name = "probe"
version = "0.2.0"
description = "Two stages, hooks and inputs"
entry_stage = "main"

[context.regions]
task = { kind = "pinned", max_tokens = 2000, required = true, seed = "task" }
brief = { kind = "pinned", max_tokens = 500, seed = "brief" }
extra = { kind = "pinned", max_tokens = 500, seed = "extra" }
notes = { kind = "compact_history", max_tokens = 500 }

[stages.main]
mode = "autonomous"
description = "First"
system_prompt = "first"
max_iterations = 4
available_tools = ["shell"]

[[stages.main.model.models]]
provider = "openai"
model = "gpt-mock"

[[stages.main.model.models]]
provider = "other"
model = "m2"

[stages.main.model.parameters]
max_output_tokens = 4096

[stages.main.hooks]
on_stage_enter = "hooks/a.rhai"
on_stage_exit = "hooks/a.rhai"
before_inference = "hooks/b.rhai"
after_inference = "hooks/missing.rhai"

[stages.main.security]
taint_tracking = true

[stages.second]
mode = "autonomous"
description = "Second"
system_prompt = "second"
max_iterations = 2

[[stages.second.model.models]]
provider = ""
model = "any-model"

[stages.third]
mode = "autonomous"
description = "Third"
system_prompt = "third"
max_iterations = 2

[stages.third.model.parameters]
max_output_tokens = "25%"
"#;

/// The rich blueprint installed beside the run, with its scripts, and the
/// run pointed at it.
fn rich(run: &Run) {
    let agent = run.dir.parent().unwrap().join("agent");
    std::fs::create_dir_all(agent.join("hooks")).unwrap();
    std::fs::write(agent.join("agent.leviath"), RICH).unwrap();
    std::fs::write(agent.join("hooks/a.rhai"), "fn on_stage_enter(ctx) { ctx }").unwrap();
    std::fs::write(agent.join("hooks/b.rhai"), "fn on_stage_enter(ctx) { ctx }").unwrap();
    run.remove("blueprint.leviath");
    let path = agent.join("agent.leviath").display().to_string();
    run.meta(|m| {
        m.agent_path.clone_from(&path);
        m.num_stages = 3;
    });
}

/// Set the run's status everywhere, dropping the journal's own status records.
fn status(run: &Run, status: OldStatus, waiting: Option<WaitReason>, error: Option<&str>) {
    run.journal(|r| r.retain(|r| !matches!(r, JournalRecord::StatusChanged { .. })));
    run.meta(|m| {
        m.status = status.clone();
        m.waiting_on = waiting.clone();
        m.error = error.map(str::to_string);
    });
}

#[test]
fn a_rich_run_carries_its_scripts_inputs_and_stages() {
    let run = Run::fixture("finished");
    rich(&run);
    run.journal(|records| {
        let JournalRecord::ContextCheckpoint { snapshot, .. } = &mut records[1] else {
            panic!("the second record is the first checkpoint");
        };
        let mut brief = snapshot.regions[0].clone();
        brief.name = "brief".into();
        brief.entries[0].content = "the brief".into();
        snapshot.regions.push(brief);
    });
    run.meta(|m| {
        m.current_stage = "second".into();
        m.stage_index = 1;
        m.model = None;
        m.blueprint_digest = Some("not a digest".into());
        m.unattended = Unattended::Profile(ProfileName::new("careful").unwrap());
        m.model_override = Some("openai/gpt-mock".into());
        m.callback_url = Some("https://example.com/hook".into());
        m.callback_secret = Some("shh".into());
        m.metadata.insert("team".into(), "a".into());
        m.max_child_depth = 3;
        m.output_request = Some(
            serde_json::from_value(json!({
                "format": "json",
                "validator": "check.rhai",
                "artifacts": [
                    {"name": "chart", "type": "image/png"},
                    {"name": "bad", "type": "not a mime"}
                ]
            }))
            .unwrap(),
        );
    });
    // It started in the entry stage, as every run does, and moved on.
    run.journal(|records| {
        let JournalRecord::Header { meta, .. } = &mut records[0] else {
            panic!("the journal starts with its header");
        };
        meta.current_stage = "main".into();
    });
    let (report, file) = run.converted();
    assert!(matches!(report.blueprint, BlueprintSource::Installed(_)));
    let spec = &file.spec;
    assert_eq!(spec.code.len(), 2);
    assert_eq!(
        file.code.len(),
        1,
        "two scripts with the same bytes are stored once"
    );
    assert!(report.defaulted("code.hooks/missing.rhai").is_some());
    assert_eq!(
        spec.inputs.get("brief"),
        Some(&InputValue::Text("the brief".into()))
    );
    assert!(spec.inputs.get("extra").is_none());
    assert!(report.defaulted("inputs.extra").is_some());
    assert!(spec.seeded.contains_key("brief"));
    // A history region with no source converts as one, not as a stand-in.
    assert!(report.defaulted("layout.regions.notes.kind").is_none());
    let notes = spec
        .graph
        .layout
        .regions
        .iter()
        .find(|r| r.name.as_str() == "notes");
    assert_eq!(
        notes.map(|r| &r.kind),
        Some(&leviath_runtime::spec::graph::RegionKind::CompactHistory { source: None })
    );
    assert!(report.defaulted("origin.blueprint.digest").is_some());
    let main = spec.stage("main").unwrap();
    assert_eq!(main.max_output_tokens, Some(4096));
    assert_eq!(main.fallbacks.len(), 1);
    assert_eq!(main.fallbacks[0].model.as_str(), "m2");
    // A stage that takes the caller's model runs on the one the run was
    // launched with.
    let second = spec.stage("second").unwrap();
    assert_eq!(second.provider.as_str(), "openai");
    assert_eq!(second.model.as_str(), "gpt-mock");
    assert!(report.defaulted("stages.second.model").is_none());
    assert_eq!(spec.stage("third").unwrap().max_output_tokens, None);
    assert!(report.defaulted("stages.third.max_output_tokens").is_some());
    assert!(matches!(&spec.launch.unattended, Unattended::Profile(p) if p.as_str() == "careful"));
    assert_eq!(spec.launch.max_depth, 3);
    assert_eq!(
        spec.requested_model.as_ref().unwrap().model.as_str(),
        "gpt-mock"
    );
    let out = spec.requested_output.as_ref().unwrap();
    assert_eq!(out.artifacts.len(), 1);
    assert!(out.validator.is_some());
    let cb = spec.delivery.callback.as_ref().unwrap();
    assert_eq!(cb.secret.as_ref().unwrap().expose(), "shh");
    assert_eq!(spec.delivery.metadata["team"], "a");
    assert_eq!(file.last.cursor.stage.as_str(), "second");
    assert_eq!(file.fold(), file.last);
    let region = file.states[0].context.region("task").unwrap();
    assert!(region.taint.is_some(), "the entry stage tracks taint");
}

/// A stage whose only model names no provider, in a run launched with no
/// model, has nothing to run on until something looks one up, and the report
/// says so.
#[test]
fn a_stage_with_a_bare_model_and_no_launch_model_is_named_unknown() {
    let run = Run::fixture("finished");
    rich(&run);
    let (report, file) = run.converted();
    let second = file.spec.stage("second").unwrap();
    assert_eq!(second.provider.as_str(), "unknown");
    assert!(report.defaulted("stages.second.model").is_some());
}

#[test]
fn launch_settings_that_do_not_check_are_named() {
    let run = Run::fixture("finished");
    run.meta(|m| {
        m.model_override = Some("has space/model".into());
        m.callback_url = Some("ftp://nope".into());
        m.children = vec!["ok-child".into(), "bad/child".into()];
        m.blueprint_digest = None;
    });
    run.json("meta.json", |m| m["yolo_profile"] = " bad".into());
    let (report, file) = run.converted();
    assert_eq!(file.spec.launch.unattended, Unattended::Off);
    assert!(report.defaulted("launch.unattended").is_some());
    assert!(file.spec.requested_model.is_none());
    assert!(file.spec.delivery.callback.is_none());
    assert_eq!(file.last.children.len(), 1);
    for word in ["launch model", "callback", "child run"] {
        assert!(report.notes.iter().any(|n| n.contains(word)), "{word}");
    }
}

#[test]
fn every_old_status_reads_as_a_new_one() {
    let cases = [
        (
            OldStatus::Starting,
            None,
            None,
            RunStatus::Idle,
            PipelinePhase::ReadyToInfer,
        ),
        (
            OldStatus::CompleteInteractive,
            None,
            None,
            RunStatus::Complete,
            PipelinePhase::Done,
        ),
        (
            OldStatus::Paused,
            None,
            None,
            RunStatus::Paused,
            PipelinePhase::Paused,
        ),
        (
            OldStatus::Cancelled,
            None,
            None,
            RunStatus::Cancelled,
            PipelinePhase::Done,
        ),
        (
            OldStatus::Error,
            None,
            None,
            RunStatus::Error("the run failed and did not say why".into()),
            PipelinePhase::Done,
        ),
        (
            OldStatus::WaitingInput,
            Some(WaitReason::Children { outstanding: 1 }),
            None,
            RunStatus::Waiting,
            PipelinePhase::WaitingForChildren,
        ),
    ];
    for (old, waiting, error, new, phase) in cases {
        let run = Run::fixture("finished");
        status(&run, old, waiting, error);
        let (_, file) = run.converted();
        assert_eq!(file.last.status, new);
        assert_eq!(file.last.phase, phase);
        assert_eq!(file.fold(), file.last);
    }
}

#[test]
fn the_stage_ledger_reads_every_status_and_skips_what_does_not_check() {
    let run = Run::fixture("finished");
    run.json("stages.json", |v| {
        let first = v[0].clone();
        let mut pending = first.clone();
        pending["name"] = json!("later");
        pending["status"] = json!("pending");
        pending["models"] = json!([{"provider": "bad provider", "model": "m"}]);
        let mut skipped = first.clone();
        skipped["name"] = json!("skipped");
        skipped["status"] = json!("skipped");
        skipped["visits"] = json!([]);
        skipped["visit_count"] = json!(0);
        let mut paused = first.clone();
        paused["name"] = json!("paused");
        paused["status"] = json!("paused");
        let mut cancelled = first.clone();
        cancelled["name"] = json!("cancelled");
        cancelled["status"] = json!("cancelled");
        let mut bad = first;
        bad["name"] = json!(" bad");
        v.as_array_mut()
            .unwrap()
            .extend([pending, skipped, paused, cancelled, bad]);
    });
    let (_, file) = run.converted();
    let statuses: Vec<StageStatus> = file.last.ledger.iter().map(|r| r.status).collect();
    assert_eq!(
        statuses,
        vec![
            StageStatus::Complete,
            StageStatus::Pending,
            StageStatus::Skipped,
            StageStatus::Paused,
            StageStatus::Cancelled
        ]
    );
    assert!(file.last.ledger[1].models.is_empty());
    assert!(!file.last.visits.contains_key("skipped"));
}

/// An old record files the stage a run was cancelled in as failed, and the
/// stage a paused run was in as running. Converted, that stage reads as its
/// run does.
#[test]
fn the_stage_a_run_stopped_in_converts_as_its_run_stands() {
    for (old, recorded, new) in [
        (OldStatus::Cancelled, "error", StageStatus::Cancelled),
        (OldStatus::Paused, "active", StageStatus::Paused),
        (OldStatus::Error, "error", StageStatus::Error),
    ] {
        let run = Run::fixture("finished");
        status(&run, old.clone(), None, None);
        run.json("stages.json", |v| v[0]["status"] = json!(recorded));
        let (_, file) = run.converted();
        let here = file
            .last
            .ledger
            .iter()
            .find(|r| r.stage == file.last.cursor.stage)
            .map(|r| r.status);
        assert_eq!(here, Some(new), "{old:?}");
        assert_eq!(file.fold(), file.last);
    }
}

/// A record from before stages said whether they were entered names none,
/// so whether a stage was reached is read off what the record says it did:
/// its spend, its visits, when it started, or a status past pending. Such
/// a stage keeps the status its record gives it, a stage it never reached
/// keeps reading pending, as every earlier release showed both, and the
/// stage a cancelled run stopped in reads cancelled.
#[test]
fn a_stage_from_before_entered_was_recorded_keeps_its_status() {
    for (run_status, here) in [
        (OldStatus::Complete, StageStatus::Complete),
        (OldStatus::Error, StageStatus::Error),
        (OldStatus::Cancelled, StageStatus::Cancelled),
    ] {
        let run = Run::fixture("finished");
        rich(&run);
        status(&run, run_status.clone(), None, None);
        run.json("stages.json", |v| {
            let first = v[0].clone();
            let bare = |name: &str, status: &str| {
                let mut s = first.clone();
                s["name"] = json!(name);
                s["status"] = json!(status);
                s["prompt_tokens"] = json!(0);
                s["completion_tokens"] = json!(0);
                s["visits"] = json!([]);
                s["visit_count"] = json!(0);
                s["started_at"] = json!(null);
                s
            };
            let mut main = first.clone();
            main["status"] = json!("error");
            let second = bare("second", "complete");
            let mut third = bare("third", "pending");
            third["started_at"] = first["started_at"].clone();
            let mut visited = bare("visited", "pending");
            visited["visit_count"] = json!(1);
            let never = bare("never", "pending");
            *v = json!([main, second, third, visited, never]);
            for s in v.as_array_mut().unwrap() {
                s.as_object_mut().unwrap().remove("entered");
            }
        });
        let (_, file) = run.converted();
        let read: Vec<(String, StageStatus, bool)> = file
            .last
            .ledger
            .iter()
            .map(|r| (r.stage.to_string(), r.status, r.entered))
            .collect();
        assert_eq!(
            read,
            vec![
                ("main".into(), here, true),
                ("second".into(), StageStatus::Complete, true),
                ("third".into(), StageStatus::Pending, true),
                // A visit alone is not entering: a fan-out worker was
                // placed in its entry stage before it moved to its own.
                ("visited".into(), StageStatus::Pending, false),
                ("never".into(), StageStatus::Pending, false),
            ],
            "{run_status:?}"
        );
        assert_eq!(file.fold(), file.last);
    }
}

#[test]
fn a_run_in_no_stage_it_names_resumes_at_the_entry() {
    let run = Run::fixture("finished");
    run.meta(|m| m.current_stage = String::new());
    let (report, file) = run.converted();
    assert_eq!(file.last.cursor.stage.as_str(), "main");
    assert!(report.defaulted("cursor.stage").is_some());
}

/// A run whose record names no stage entered none, and is listed with none;
/// one that names its stage was in it, ledger or not.
#[test]
fn a_run_that_named_no_stage_is_listed_in_none() {
    let run = Run::fixture("finished");
    run.remove("stages.json");
    run.meta(|m| m.current_stage = String::new());
    let (_, file) = run.converted();
    assert!(file.last.visits.is_empty());
    let listed = leviath_runtime::runfile::summary_of(&file.spec, &file.last, 0);
    assert_eq!(listed.current_stage, "");

    let named = Run::fixture("finished");
    named.remove("stages.json");
    let (_, file) = named.converted();
    assert_eq!(file.last.visits.get("main"), Some(&1));
    let listed = leviath_runtime::runfile::summary_of(&file.spec, &file.last, 0);
    assert_eq!(listed.current_stage, "main");
}

/// Every point of a converted run's history names the stage the run was in
/// there, as the release that wrote it showed it: the stage it started in is
/// visited from the start, and each move to another stage is a visit.
#[test]
fn every_point_of_a_converted_history_names_its_stage() {
    let run = Run::fixture("finished");
    rich(&run);
    run.journal(|records| {
        let first = records
            .iter_mut()
            .find(|r| matches!(r, JournalRecord::Progress { .. }))
            .unwrap();
        let JournalRecord::Progress { meta, .. } = first else {
            unreachable!()
        };
        meta.current_stage = "second".into();
    });
    let (_, file) = run.converted();
    assert_eq!(file.fold(), file.last);
    let mut state = file.states[0].clone();
    let name = |s: &leviath_runtime::state::RunState| {
        leviath_runtime::runfile::summary_of(&file.spec, s, 0).current_stage
    };
    let mut named = vec![(name(&state), state.visits.clone())];
    for d in &file.deltas {
        d.apply(&mut state);
        named.push((name(&state), state.visits.clone()));
    }
    let visits = |pairs: &[(&str, u32)]| -> std::collections::BTreeMap<_, _> {
        pairs
            .iter()
            .map(|(s, n)| {
                (
                    leviath_runtime::spec::names::StageName::new(*s).unwrap(),
                    *n,
                )
            })
            .collect()
    };
    assert_eq!(named[0], ("main".into(), visits(&[("main", 1)])));
    assert!(named.iter().all(|(n, _)| !n.is_empty()), "{named:?}");
    assert!(
        named.contains(&("second".into(), visits(&[("main", 1), ("second", 1)]))),
        "{named:?}"
    );
    assert!(
        named.contains(&("main".into(), visits(&[("main", 2), ("second", 1)]))),
        "{named:?}"
    );
}

/// When the run last made progress is kept where its record says it was
/// earlier than the record's last write, as a reaped worker's is: its
/// parent touches `meta.json` and nothing else.
#[test]
fn a_record_touched_after_its_last_progress_keeps_when_that_was() {
    let run = Run::fixture("finished");
    let mut at = 0;
    run.json("meta.json", |m| {
        at = m["updated_at"].as_i64().unwrap() - 130;
        m["last_progress_at"] = at.into();
    });
    let (_, file) = run.converted();
    assert_eq!(file.last.last_progress_at, Some(at));
    let updated = file.deltas.last().unwrap().at;
    let listed = leviath_runtime::runfile::summary_of(&file.spec, &file.last, updated);
    assert_eq!(listed.last_progress_at, Some(at));
    assert_eq!(listed.updated_at, updated);

    let moved = Run::fixture("finished");
    moved.meta(|m| m.last_progress_at = Some(m.updated_at));
    let (_, file) = moved.converted();
    assert_eq!(
        file.last.last_progress_at, None,
        "its last step is its progress"
    );
}

#[test]
fn context_entries_of_every_kind_are_typed() {
    let run = Run::fixture("finished");
    run.journal(|records| {
        let JournalRecord::ContextCheckpoint { snapshot, .. } = &mut records[1] else {
            panic!("the second record is the first checkpoint");
        };
        let entry = |v: serde_json::Value| serde_json::from_value(v).unwrap();
        let blob = |sha: &str| {
            json!({"sha256": sha, "mime_type": "image/png", "size": 3, "tokens": 5, "stand_in": "[image]"})
        };
        let good = "a".repeat(64);
        let region = &mut snapshot.regions[0];
        region.entries.extend([
            entry(json!({"content": "hi", "tokens": 1, "kind": {"type": "UserMessage"}})),
            entry(json!({"content": "do it", "tokens": 1,
                "metadata": {"checklist_id": 1, "checklist_done": true, "checklist_note": "ok"}})),
            entry(json!({"content": "x", "tokens": 1, "metadata": {"origin": "test"}})),
            entry(json!({"content": [
                {"mime_type": "text/markdown", "body": "# hi"},
                {"mime_type": "image/png", "body": blob(&good)},
                {"mime_type": "image/png", "body": blob("nope")}
            ], "tokens": 9})),
            entry(json!({"content": [{"mime_type": "text/plain", "body": blob(&good)}], "tokens": 5})),
        ]);
        let mut bad = region.clone();
        bad.name = " bad".into();
        snapshot.regions.push(bad);
    });
    // The part's file is in the blob directory too: it is named once, as its
    // context holds it.
    let good = "a".repeat(64);
    std::fs::create_dir_all(run.path("blobs")).unwrap();
    std::fs::write(run.path("blobs").join(&good), b"png").unwrap();
    let (report, file) = run.converted();
    let named: Vec<_> = file
        .last
        .blobs
        .iter()
        .filter(|b| b.digest.as_str() == good)
        .collect();
    assert_eq!(named.len(), 1);
    assert_eq!(named[0].mime_type, "image/png");
    let task = &file.states[0].context.region("task").unwrap().entries;
    assert_eq!(task[1].kind, EntryKind::UserMessage);
    assert_eq!(
        task[2].meta,
        EntryMeta::ChecklistItem {
            id: 1,
            done: true,
            note: Some("ok".into())
        }
    );
    assert_eq!(task[3].meta, EntryMeta::None);
    let parts = &task[4].parts;
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0].body, PartBody::Inline("# hi".into()));
    assert!(matches!(&parts[1].body, PartBody::Stored(b) if b.size == 3));
    assert_eq!(parts[2].body, PartBody::Inline("[image]".into()));
    assert_eq!(task[5].parts.len(), 1, "a stored text part is still a part");
    assert!(report.defaulted("context.entries.meta").is_some());
    assert!(report.notes.iter().any(|n| n.contains("\" bad\"")));
    assert!(report.notes.iter().any(|n| n.contains("\"nope\"")));
}

/// Stored parts stay in `blobs/`, where a run keeps them, and the run file
/// names each one rather than holding its bytes. A file there whose name is
/// not a digest is not named, and the report says so.
#[test]
fn stored_parts_stay_beside_the_run_file_and_are_named() {
    let run = Run::fixture("finished");
    let blobs = run.path("blobs");
    std::fs::create_dir_all(&blobs).unwrap();
    let digest = leviath_runtime::spec::names::Digest::of(b"png");
    std::fs::write(blobs.join(digest.as_str()), b"png").unwrap();
    std::fs::write(blobs.join("notes.txt"), b"x").unwrap();
    let (report, file) = run.converted();
    let named: Vec<_> = file
        .last
        .blobs
        .iter()
        .map(|b| {
            (
                b.digest.clone(),
                b.size,
                b.mime_type.as_str(),
                b.region.is_none(),
            )
        })
        .collect();
    assert_eq!(
        named,
        vec![(digest.clone(), 3, "application/octet-stream", true)]
    );
    assert!(report.notes.iter().any(|n| n.contains("blobs/notes.txt")));
    assert!(blobs.join(digest.as_str()).is_file());
    assert!(!report.legacy_dir.join("blobs").exists());
    let bytes = std::fs::read(run.path("run.lvr")).unwrap();
    let reader =
        leviath_runtime::runfile::RunFileReader::from_bytes(&run.path("run.lvr"), bytes).unwrap();
    assert_eq!(reader.blob(&digest).unwrap(), b"png");
}

#[test]
fn a_fan_out_and_an_open_point_that_do_not_fit_are_named() {
    let run = Run::fixture("fanout-parent");
    run.json("fanout.json", |v| {
        v["active"] = json!([["alpha", "bad/run"]]);
        v["parts"] = json!([{"mime_type": "text/plain", "body": "x"}]);
        v["pending"] = json!([]);
        v["origin"] = json!(null);
    });
    run.write(
        "interactions.json",
        r#"{"cursor": 3, "round": 0, "body": ""}"#,
    );
    let (report, file) = run.converted();
    let fan = file.last.fan_out.as_ref().unwrap();
    assert!(fan.active.is_empty());
    assert!(fan.queued.is_empty());
    assert!(!report.notes.iter().any(|n| n.contains("origin")));
    assert!(report.notes.iter().any(|n| n.contains("\"bad/run\"")));
    assert!(report.notes.iter().any(|n| n.contains("1 files")));
    assert_eq!(file.last.interactions[0].prompt, "");
    assert!(report.defaulted("interactions[0].prompt").is_some());
}

/// A fan-out its stage declares resumes with the stage's own settings, and
/// nothing is said about them.
#[test]
fn a_declared_fan_out_keeps_its_settings() {
    let run = Run::fixture("fanout-parent");
    let text = std::fs::read_to_string(run.path("blueprint.leviath")).unwrap();
    run.write(
        "blueprint.leviath",
        &text.replace(
            "mode = \"autonomous\"",
            "mode = \"fan_out\"\nworker_agent = \"waiter\"\nmax_workers = 3",
        ),
    );
    let (report, file) = run.converted();
    let fan = file.last.fan_out.as_ref().unwrap();
    assert_eq!(fan.config.max_workers, Some(3));
    assert!(
        !report
            .notes
            .iter()
            .any(|n| n.contains("resumes with that stage"))
    );
}

/// An old run whose fan-out had no cap (`max_workers = 0`) keeps no cap, the
/// way a graph writes it, and the conversion report says so.
#[test]
fn a_fan_out_with_no_cap_converts_to_one_and_says_so() {
    let run = Run::fixture("fanout-parent");
    let text = std::fs::read_to_string(run.path("blueprint.leviath")).unwrap();
    run.write(
        "blueprint.leviath",
        &text.replace(
            "mode = \"autonomous\"",
            "mode = \"fan_out\"\nworker_agent = \"waiter\"\nmax_workers = 0",
        ),
    );
    let (report, file) = run.converted();
    let fan = file.last.fan_out.as_ref().unwrap();
    assert_eq!(fan.config.max_workers, None);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("max_workers = 0 (no cap) is left out")),
        "{:?}",
        report.notes
    );
}

#[test]
fn a_final_output_without_its_file_or_stage_is_named() {
    let run = Run::fixture("real-finished");
    run.remove("final_output");
    run.meta(|m| {
        if let Some(out) = m.final_output.as_mut() {
            out.stage = String::new();
            out.artifacts = vec![
                serde_json::from_value(
                    json!({"name": "a.png", "path": "a.png", "mime_type": "image/png"}),
                )
                .unwrap(),
            ];
        }
    });
    let (report, file) = run.converted();
    let out = file.last.final_output.as_ref().unwrap();
    assert_eq!(out.bytes, 0);
    assert_eq!(file.last.files.final_output, None);
    assert_eq!(out.stage.as_str(), "engine");
    assert!(report.defaulted("final_output.bytes").is_some());
    // The files it handed back are kept with it.
    assert_eq!(out.artifacts.len(), 1);
    assert_eq!(out.artifacts[0].path, "a.png");
}

#[test]
fn a_run_from_before_the_journal_converts_from_its_json_files() {
    let run = Run::fixture("finished");
    run.remove("run.lvr");
    let (_, file) = run.converted();
    assert_eq!(file.last.status, RunStatus::Complete);
    assert!(file.last.context.region("task").is_some());
    let bare = Run::fixture("finished");
    bare.remove("run.lvr");
    bare.remove("context.json");
    let (_, file) = bare.converted();
    assert!(file.states[0].context.regions.is_empty());
    assert_eq!(file.fold(), file.last);
}

#[test]
fn the_blueprint_is_found_where_the_run_points() {
    let run = Run::fixture("interaction-worker");
    let agent = crate::common::fixtures_dir().join("agents/waiter");
    run.meta(|m| m.agent_path = agent.display().to_string());
    let no_env = convert(&run.dir, &ConvertEnv::default()).unwrap();
    assert_eq!(
        no_env.blueprint,
        BlueprintSource::Installed(agent.join("agent.leviath"))
    );
}

#[test]
fn broken_directories_are_refused_by_name() {
    let empty = tempfile::tempdir().unwrap();
    let err = convert(empty.path(), &ConvertEnv::default()).unwrap_err();
    assert!(matches!(err, ConvertError::NotARun { .. }));
    assert!(err.to_string().contains("has no meta.json"));

    type BreakIt = fn(&Run);
    let cases: [(&str, BreakIt, &str); 10] = [
        ("bad meta", |r| r.write("meta.json", "{"), "does not parse"),
        (
            "bad stages",
            |r| r.write("stages.json", "{"),
            "does not parse",
        ),
        (
            "bad fan-out",
            |r| r.write("fanout.json", "{"),
            "does not parse",
        ),
        (
            "bad point",
            |r| r.write("interactions.json", "{"),
            "does not parse",
        ),
        (
            "bad context without a journal",
            |r| {
                r.remove("run.lvr");
                r.write("context.json", "{");
            },
            "does not parse",
        ),
        (
            "bad journal",
            |r| r.write("run.lvr", "nope"),
            "neither an LVR1",
        ),
        (
            "torn journal",
            |r| r.write("run.lvr", "LVR1"),
            "does not parse",
        ),
        (
            "headless journal",
            |r| r.set_records(&[]),
            "does not start with its header",
        ),
        (
            "bad run id",
            |r| r.meta(|m| m.run_id = "a/b".into()),
            "run_id is not valid",
        ),
        (
            "bad parent",
            |r| r.meta(|m| m.parent_run_id = Some("a/b".into())),
            "parent_run_id is not valid",
        ),
    ];
    for (what, break_it, says) in cases {
        let run = Run::fixture("finished");
        break_it(&run);
        let err = run.convert().unwrap_err();
        assert!(err.to_string().contains(says), "{what}: {err}");
        assert!(run.path("meta.json").is_file(), "{what}: nothing moved");
    }
    let run = Run::fixture("finished");
    run.meta(|m| m.agent_name = " bad".into());
    assert!(
        run.convert()
            .unwrap_err()
            .to_string()
            .contains("agent_name is not valid")
    );
}

#[test]
fn files_that_cannot_be_read_or_moved_are_named() {
    // A directory where a stored part would be is not one, and is said so.
    let run = Run::fixture("finished");
    let digest = leviath_runtime::spec::names::Digest::of(b"x");
    std::fs::create_dir_all(run.path("blobs").join(digest.as_str())).unwrap();
    let (report, file) = run.converted();
    assert!(file.last.blobs.is_empty());
    let note = format!("blobs/{digest} is not named in the run file");
    assert!(
        report.notes.iter().any(|n| n.contains(&note)),
        "{:?}",
        report.notes
    );

    let run = Run::fixture("finished");
    run.write("legacy", "in the way");
    assert!(matches!(
        run.convert().unwrap_err(),
        ConvertError::Io { .. }
    ));
}

#[test]
fn a_report_reads_as_lines() {
    let (report, _) = Run::fixture("finished").converted();
    let env = report.defaulted("env").unwrap();
    assert!(env.to_string().starts_with("env = empty ("));
    assert!(report.defaulted("nothing").is_none());
}

#[test]
fn a_task_the_graph_does_not_take_is_noted() {
    let run = Run::fixture("finished");
    let text = std::fs::read_to_string(run.path("blueprint.leviath")).unwrap();
    let task = "task = { kind = \"pinned\", max_tokens = 2000, required = true, seed = \"task\" }";
    assert!(text.contains(task));
    run.write(
        "blueprint.leviath",
        &text.replace(task, "goal = { kind = \"pinned\", max_tokens = 2000 }"),
    );
    let (report, file) = run.converted();
    assert!(file.spec.inputs.0.is_empty());
    assert!(report.notes.iter().any(|n| n.contains("not an input")));
}

/// A region key the old parser read nothing from, such as `max_stored`,
/// converts as if it were not there, and the report names it with its value.
#[test]
fn a_key_the_old_parser_ignored_is_left_out_and_reported() {
    let run = Run::fixture("finished");
    let text = std::fs::read_to_string(run.path("blueprint.leviath")).unwrap();
    let text = text.replace("seed = \"task\" }", "seed = \"task\", max_stored = 4 }");
    run.write("blueprint.leviath", &text);
    let (report, _) = run.converted();
    let notes = &report.notes;
    let dropped: Vec<String> = report.dropped.iter().map(ToString::to_string).collect();
    assert_eq!(report.blueprint_name, "probe");
    assert!(
        dropped
            .iter()
            .any(|n| n.contains("region 'task'") && n.contains("`max_stored = 4`")),
        "{dropped:?}"
    );
    assert!(
        !notes
            .iter()
            .any(|n| n.contains("graph is what it recorded")),
        "{notes:?}"
    );
}

/// A conversion killed while it moved the old files aside leaves the
/// metadata and the old journal in `legacy/` and no run file. Putting the run
/// back makes it an old run again, which converts as if nothing had happened;
/// an old run, a converted one and a run whose files cannot all move back
/// are left as they are.
#[test]
fn a_conversion_stopped_part_way_is_put_back_and_converts_again() {
    use leviath_legacy_runs::{is_legacy, put_back};
    let whole = Run::fixture("finished");
    let (_, expected) = whole.converted();

    let run = Run::fixture("finished");
    assert!(!put_back(&run.dir).unwrap(), "an old run is left as it is");
    let legacy = run.path("legacy");
    std::fs::create_dir_all(&legacy).unwrap();
    for name in ["meta.json", "run.lvr"] {
        std::fs::rename(run.path(name), legacy.join(name)).unwrap();
    }
    run.write("run.lvr.converting", "half a run file");
    assert!(!is_legacy(&run.dir));
    assert!(put_back(&run.dir).unwrap());
    assert!(is_legacy(&run.dir));
    assert!(!legacy.exists());
    assert!(!run.path("run.lvr.converting").exists());
    let (_, file) = run.converted();
    assert_eq!(file.spec, expected.spec);
    assert!(
        !put_back(&run.dir).unwrap(),
        "a converted run is left as it is"
    );

    let clash = Run::fixture("finished");
    let legacy = clash.path("legacy");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::rename(clash.path("run.lvr"), legacy.join("run.lvr")).unwrap();
    std::fs::copy(clash.path("meta.json"), legacy.join("meta.json")).unwrap();
    assert!(put_back(&clash.dir).is_err());
    assert!(legacy.join("meta.json").is_file());
}
