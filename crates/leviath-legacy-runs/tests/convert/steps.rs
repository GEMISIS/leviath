//! What each step of a converted run holds: the stored parts its run had by
//! then, and a report of whatever reading the old journal left out.

use leviath_legacy_runs::journal::JournalRecord;
use leviath_runtime::spec::names::{Digest, RegionName};
use leviath_runtime::state::{BlobFile, RunEvent, RunState};
use serde_json::{Value, json};

use crate::common::{Run, journal_bytes, raw_frames};

/// A stored part's body as an old snapshot held it.
fn stored(digest: &Digest, mime: &str) -> Value {
    json!({"sha256": digest.as_str(), "mime_type": mime, "size": 3, "tokens": 5, "stand_in": "[part]"})
}

/// A window diff that sets or appends entries in one region.
fn diff(region: Value, at: i64) -> JournalRecord {
    serde_json::from_value(json!({"ContextDiff": {
        "delta": {"stage_name": "main", "total_tokens": 9, "max_tokens": 2000, "regions": [region]},
        "at": at
    }}))
    .unwrap()
}

/// Every state of the run, from the one it started in to the last.
fn every_state(run: &Run) -> Vec<RunState> {
    let (_, file) = run.converted();
    let mut state = file.states[0].clone();
    let mut out = vec![state.clone()];
    for d in &file.deltas {
        d.apply(&mut state);
        out.push(state.clone());
    }
    assert_eq!(state, file.last);
    out
}

fn named<'a>(state: &'a RunState, digest: &Digest) -> Option<&'a BlobFile> {
    state.blobs.iter().find(|b| b.digest == *digest)
}

/// A part stored part way through a run is named at every step from the one
/// it appeared in, as a run in the new layout names it: its type, its name,
/// the region it appeared in and the tool that returned it. The window
/// letting it go later does not unname it, and its file is not mistaken for
/// a stray one.
#[test]
fn a_part_stored_mid_run_is_named_from_the_step_it_appeared_in() {
    let run = Run::fixture("finished");
    let chart = Digest::of(b"png");
    let clip = Digest::of(b"wav");
    run.journal(|records| {
        let more = [
            diff(
                json!({"Append": {"name": "task", "current_tokens": 9, "entries": [{
                    "content": [{"mime_type": "image/png", "name": "chart.png", "body": stored(&chart, "image/png")}],
                    "tokens": 5
                }]}}),
                1_790_811_836,
            ),
            diff(
                json!({"Append": {"name": "tool_results", "current_tokens": 5, "entries": [{
                    "content": [{"mime_type": "audio/wav", "name": "clip.wav", "body": stored(&clip, "audio/wav")}],
                    "tokens": 5,
                    "kind": {"type": "ToolResult", "tool_call_id": "call_1", "tool_name": "render", "is_error": false}
                }]}}),
                1_790_811_836,
            ),
            // The task goes back to its text alone, and the chart with it.
            diff(
                json!({"Set": {"name": "task", "kind": "pinned", "current_tokens": 4, "max_tokens": 2000,
                    "entries": [{"content": "What time is it?", "tokens": 4}]}}),
                1_790_811_836,
            ),
        ];
        records.splice(2..2, more);
    });
    std::fs::create_dir_all(run.path("blobs")).unwrap();
    for (digest, bytes) in [(&chart, b"png"), (&clip, b"wav")] {
        std::fs::write(run.path("blobs").join(digest.as_str()), bytes).unwrap();
    }
    let states = every_state(&run);
    let first = states
        .iter()
        .position(|s| named(s, &chart).is_some())
        .expect("the chart is named once it is stored");
    assert!(first > 0, "the run did not start with the chart");
    assert!(
        states[first]
            .context
            .region("task")
            .unwrap()
            .entries
            .iter()
            .any(|e| !e.parts.is_empty()),
        "it is named in the step that stored it"
    );
    let chart_file = BlobFile {
        digest: chart.clone(),
        mime_type: "image/png".into(),
        size: 3,
        name: Some("chart.png".into()),
        region: Some(RegionName::new("task").unwrap()),
        tool: None,
    };
    let clip_file = BlobFile {
        digest: clip.clone(),
        mime_type: "audio/wav".into(),
        size: 3,
        name: Some("clip.wav".into()),
        region: Some(RegionName::new("tool_results").unwrap()),
        tool: Some("render".into()),
    };
    for (seq, state) in states.iter().enumerate().skip(first) {
        assert_eq!(named(state, &chart), Some(&chart_file), "step {seq}");
    }
    assert!(named(&states[first], &clip).is_none());
    for (seq, state) in states.iter().enumerate().skip(first + 1) {
        assert_eq!(named(state, &clip), Some(&clip_file), "step {seq}");
    }
    let last = states.last().unwrap();
    let held_in_task = last
        .context
        .region("task")
        .unwrap()
        .entries
        .iter()
        .any(|e| !e.parts.is_empty());
    assert!(!held_in_task, "the window let the chart go");
    assert_eq!(last.blobs.len(), 2, "{:?}", last.blobs);
}

/// Records of the old journal that do not read, and a record a crash cut
/// short at its end, are left out, and the report says how many, as does the
/// run's own log.
#[test]
fn records_left_out_of_the_journal_are_reported() {
    let run = Run::fixture("finished");
    let mut frames = raw_frames(&std::fs::read(run.path("run.lvr")).unwrap());
    frames.insert(2, br#"{"NotARecord": {"at": 1}}"#.to_vec());
    frames.insert(4, b"{ not json".to_vec());
    let mut bytes = journal_bytes(&frames);
    bytes.extend(100u64.to_be_bytes());
    bytes.extend(br#"{"Message""#);
    std::fs::write(run.path("run.lvr"), bytes).unwrap();
    let (report, file) = run.converted();
    let skipped =
        "2 records of the old journal were left out: they are not records this build reads";
    let torn = "the old journal ends in 18 bytes of a record cut short, which were left out";
    for line in [skipped, torn] {
        assert!(
            report.notes.iter().any(|n| n == line),
            "{line}: {:?}",
            report.notes
        );
        let logged = RunEvent::Log(format!("converted from the old layout: {line}"));
        assert!(
            file.deltas.last().unwrap().events.contains(&logged),
            "{line}"
        );
    }

    let whole = Run::fixture("finished");
    let (report, _) = whole.converted();
    assert!(
        !report.notes.iter().any(|n| n.contains("left out")),
        "{:?}",
        report.notes
    );
}

/// What reading the window left out at a step is reported even when a later
/// step no longer holds it: a region whose name is not valid, and a stored
/// part whose digest is not.
#[test]
fn what_a_step_left_out_of_the_window_is_reported() {
    let run = Run::fixture("finished");
    let bad = json!({"sha256": "nope", "mime_type": "image/png", "size": 3, "tokens": 5, "stand_in": "[image]"});
    run.journal(|records| {
        let more = [
            diff(
                json!({"Set": {"name": " bad", "kind": "pinned", "current_tokens": 1, "max_tokens": 10,
                    "entries": [{"content": "x", "tokens": 1}]}}),
                1_790_811_836,
            ),
            diff(
                json!({"Append": {"name": "task", "current_tokens": 9, "entries": [{
                    "content": [{"mime_type": "image/png", "body": bad}], "tokens": 5
                }]}}),
                1_790_811_836,
            ),
            diff(json!({"Remove": {"name": " bad"}}), 1_790_811_836),
            diff(
                json!({"Set": {"name": "task", "kind": "pinned", "current_tokens": 4, "max_tokens": 2000,
                    "entries": [{"content": "What time is it?", "tokens": 4}]}}),
                1_790_811_836,
            ),
        ];
        records.splice(2..2, more);
    });
    let (report, _) = run.converted();
    assert!(
        report.notes.iter().any(|n| n.contains("\" bad\"")),
        "{:?}",
        report.notes
    );
    assert!(
        report.notes.iter().any(|n| n.contains("\"nope\"")),
        "{:?}",
        report.notes
    );
}

/// An entry whose metadata has no place in the run file is counted once,
/// however many steps held it.
#[test]
fn metadata_left_out_is_counted_once_however_many_steps_held_it() {
    let run = Run::fixture("finished");
    run.journal(|records| {
        let JournalRecord::ContextCheckpoint { snapshot, .. } = &mut records[1] else {
            panic!("the second record is the first checkpoint");
        };
        let entry = json!({"content": "x", "tokens": 1, "metadata": {"origin": "test"}});
        snapshot.regions[0]
            .entries
            .push(serde_json::from_value(entry).unwrap());
    });
    let (report, _) = run.converted();
    let meta = report.defaulted("context.entries.meta").unwrap();
    assert!(meta.why.starts_with("1 entries"), "{}", meta.why);
}
