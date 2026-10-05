//! A converted run's webhook secret is kept in the secret store and in no
//! file under the run's directory.

use crate::common::{Run, contains, journal_bytes, raw_frames};

const SECRET: &str = "shh-legacy-webhook-key";

/// A run signed as an earlier release signed it: the secret in `meta.json`
/// and in every journal record carrying the metadata.
fn signed() -> Run {
    let run = Run::fixture("real-finished");
    run.meta(|m| m.callback_url = Some("https://example.com/hook".into()));
    run.sign(SECRET);
    run.sign_journal(SECRET);
    run
}

#[test]
fn the_old_files_kept_beside_a_converted_run_no_longer_hold_its_secret() {
    let run = signed();
    // The home's backup hard-links the old files, and keeps the secret.
    let backup = run.dir.parent().unwrap().parent().unwrap().join("backup");
    std::fs::create_dir(&backup).unwrap();
    for name in ["meta.json", "run.lvr"] {
        std::fs::hard_link(run.path(name), backup.join(name)).unwrap();
    }
    let records = run.records().len();
    assert_eq!(run.files_holding(SECRET).len(), 2);

    let (report, file) = run.converted();
    let kept = file.spec.delivery.callback_secret().unwrap();
    assert_eq!(run.store().read(kept).unwrap().expose(), SECRET);
    let holding = run.files_holding(SECRET);
    assert!(holding.is_empty(), "{holding:?}");
    for name in ["meta.json", "run.lvr"] {
        let saved = std::fs::read(backup.join(name)).unwrap();
        assert!(contains(&saved, SECRET), "the backup's {name} is untouched");
    }
    // The old files still read as they did, less the secret.
    let legacy = &report.legacy_dir;
    let meta = std::fs::read_to_string(legacy.join("meta.json")).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta).unwrap();
    assert_eq!(meta["callback_url"], "https://example.com/hook");
    assert!(meta.get("callback_secret").is_none());
    let journal = std::fs::read(legacy.join("run.lvr")).unwrap();
    let read = leviath_legacy_runs::journal::read(&journal).unwrap();
    assert_eq!(read.len(), records);
}

/// A journal frame that is not a JSON record, and a torn last frame, are
/// kept as they are.
#[test]
fn a_journal_frame_that_does_not_read_is_kept_as_it_is() {
    let run = signed();
    let bytes = std::fs::read(run.path("run.lvr")).unwrap();
    let mut frames = raw_frames(&bytes);
    frames.push(b"not json".to_vec());
    let mut journal = journal_bytes(&frames);
    let torn = [0u8, 0, 0, 0, 0, 0, 0, 99, b'{'];
    journal.extend(torn);
    std::fs::write(run.path("run.lvr"), &journal).unwrap();

    let (report, _) = run.converted();
    let kept = std::fs::read(report.legacy_dir.join("run.lvr")).unwrap();
    assert!(kept.ends_with(&torn));
    let frames = raw_frames(&kept);
    assert_eq!(frames.last().unwrap(), b"not json");
    let signed = frames
        .iter()
        .filter(|f| contains(f, "callback_secret"))
        .count();
    assert_eq!(signed, 0);
    assert!(run.files_holding(SECRET).is_empty());
}

/// A run with no secret keeps its old files byte for byte.
#[test]
fn a_run_with_no_secret_keeps_its_old_files_as_they_were() {
    let run = Run::fixture("real-finished");
    let meta = std::fs::read(run.path("meta.json")).unwrap();
    let journal = std::fs::read(run.path("run.lvr")).unwrap();
    let (report, _) = run.converted();
    let legacy = &report.legacy_dir;
    assert_eq!(std::fs::read(legacy.join("meta.json")).unwrap(), meta);
    assert_eq!(std::fs::read(legacy.join("run.lvr")).unwrap(), journal);
}

/// When an old file cannot be written again, the run still converts and the
/// report says which files still hold the secret.
#[test]
fn an_old_file_that_cannot_be_written_again_is_noted() {
    let run = signed();
    // Something in the way of the new copy of `meta.json`.
    std::fs::create_dir(run.path("meta.json.scrubbing")).unwrap();
    let (report, file) = run.converted();
    assert!(file.spec.delivery.callback_secret().is_some());
    let noted = report
        .notes
        .iter()
        .filter(|n| n.contains("still hold the webhook's secret"))
        .count();
    assert_eq!(noted, 1);
}
