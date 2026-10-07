use super::*;
use crate::state::context::{BlobState, PartBody, PartState};
use crate::state::{ContextState, EntryKind, EntryMeta, EntryState, RegionState};

/// A path that stays inside the run's directory is found there; one that is
/// absolute, climbs out, names a drive or is empty is refused by name.
#[test]
fn a_path_outside_the_run_directory_is_refused() {
    let dir = Path::new("/runs/r");
    let inside = FileRef::log("stages/0/logs.log", 3);
    assert_eq!(inside.path_in(dir).unwrap(), dir.join("stages/0/logs.log"));
    for bad in [
        "",
        "/etc/passwd",
        "../other/final_output",
        "stages/./x",
        "c:/x",
        "a//b",
        "a\\..\\b",
    ] {
        let err = FileRef::log(bad, 0).path_in(dir).unwrap_err();
        assert_eq!(err, FileRefError::Outside(bad.to_string()));
        assert!(
            err.to_string().contains("outside the run's directory"),
            "{err}"
        );
    }
}

/// A file written whole reads back when its digest matches, and is refused
/// when its contents changed; a log reads while it is at least as long as
/// the run wrote it, and is refused once it is shorter; a missing file is
/// refused by its path.
#[test]
fn a_file_reads_only_as_the_run_file_names_it() {
    let dir = tempfile::tempdir().unwrap();
    let answer = FileRef::whole("final_output", b"the answer");
    assert_eq!(answer.bytes, 10);
    assert_eq!(answer.sha256, Some(Digest::of(b"the answer")));

    let missing = answer.read(dir.path()).unwrap_err();
    assert!(matches!(missing, FileRefError::Unreadable { .. }));
    assert!(
        missing
            .to_string()
            .starts_with("final_output does not read"),
        "{missing}"
    );

    std::fs::write(dir.path().join("final_output"), b"the answer").unwrap();
    assert_eq!(answer.read(dir.path()).unwrap(), b"the answer");
    std::fs::write(dir.path().join("final_output"), b"the answeR").unwrap();
    let changed = answer.read(dir.path()).unwrap_err();
    assert!(
        changed.to_string().contains("contents changed"),
        "{changed}"
    );

    let log = FileRef::log("logs.log", 5);
    std::fs::write(dir.path().join("logs.log"), b"12345 and more").unwrap();
    assert_eq!(log.read(dir.path()).unwrap().len(), 14);
    std::fs::write(dir.path().join("logs.log"), b"123").unwrap();
    let short = log.read(dir.path()).unwrap_err();
    assert!(short.to_string().contains("shorter than the 5"), "{short}");

    assert_eq!(
        FileRef::log("..", 0).read(dir.path()),
        Err(FileRefError::Outside("..".into()))
    );
}

/// Each stage's files are kept in stage order, one record per stage, and a
/// file named again replaces the one before it.
#[test]
fn stage_files_are_kept_in_order_one_record_per_stage() {
    let mut files = RunFiles::default();
    files.set_stage_file(2, StageFile::Logs, FileRef::log(StageFile::Logs.path(2), 1));
    files.set_stage_file(
        0,
        StageFile::Output,
        FileRef::log(StageFile::Output.path(0), 1),
    );
    files.set_stage_file(2, StageFile::Logs, FileRef::log(StageFile::Logs.path(2), 9));
    files.set_stage_file(
        2,
        StageFile::TaintAudit,
        FileRef::whole(StageFile::TaintAudit.path(2), b"[]"),
    );
    let indexes: Vec<u32> = files.stages.iter().map(|s| s.index).collect();
    assert_eq!(indexes, vec![0, 2]);
    assert_eq!(
        files
            .stage_file(2, StageFile::Logs)
            .map(|f| (f.path.as_str(), f.bytes)),
        Some(("stages/2/logs.log", 9))
    );
    assert_eq!(
        files.stage_file(2, StageFile::TaintAudit).unwrap().path,
        "stages/2/taint_audit.json"
    );
    assert_eq!(
        files.stage_file(0, StageFile::Output).unwrap().path,
        "stages/0/output.log"
    );
    assert_eq!(files.stage_file(0, StageFile::Logs), None);
    assert_eq!(files.stage_file(1, StageFile::Output), None);
}

fn stored(name: &str, bytes: &[u8]) -> PartState {
    PartState {
        mime_type: "image/png".into(),
        body: PartBody::Stored(BlobState {
            digest: Digest::of(bytes),
            size: bytes.len() as u64,
            width: None,
            height: None,
            duration_ms: None,
            tokens: 1,
            stand_in: String::new(),
        }),
        name: Some(name.into()),
        deliver: None,
    }
}

fn entry(kind: EntryKind, parts: Vec<PartState>) -> EntryState {
    EntryState {
        text: String::new(),
        parts,
        tokens: 0,
        timestamp: 0,
        kind,
        meta: EntryMeta::None,
        key: None,
        reasoning: None,
    }
}

/// Every stored part a context holds is listed once, with where it came
/// from: its region, and the tool whose result carried it. Text parts are
/// not files.
#[test]
fn stored_parts_are_listed_once_with_where_they_came_from() {
    let inline = PartState {
        mime_type: "text/plain".into(),
        body: PartBody::Inline("hi".into()),
        name: None,
        deliver: None,
    };
    let context = ContextState {
        regions: vec![
            RegionState {
                name: RegionName::new("task").unwrap(),
                max_tokens: 0,
                current_tokens: 0,
                needs_message_compaction: false,
                taint: None,
                entries: vec![entry(
                    EntryKind::UserMessage,
                    vec![stored("in.png", b"in"), inline],
                )],
            },
            RegionState {
                name: RegionName::new("conversation").unwrap(),
                max_tokens: 0,
                current_tokens: 0,
                needs_message_compaction: false,
                taint: None,
                entries: vec![entry(
                    EntryKind::ToolResult {
                        call_id: "c".into(),
                        tool: "generate_image".into(),
                        is_error: false,
                    },
                    vec![stored("out.png", b"out"), stored("in.png", b"in")],
                )],
            },
        ],
        ..ContextState::default()
    };
    let mut blobs = Vec::new();
    assert_eq!(note_blobs(&mut blobs, &context), 2);
    assert_eq!(note_blobs(&mut blobs, &context), 0);
    assert_eq!(blobs[0].region.as_ref().unwrap().as_str(), "task");
    assert_eq!(blobs[0].tool, None);
    assert_eq!(blobs[0].name.as_deref(), Some("in.png"));
    assert_eq!(blobs[1].tool.as_deref(), Some("generate_image"));
    assert_eq!(blobs[1].size, 3);
    assert_eq!(blobs[1].path(), format!("blobs/{}", Digest::of(b"out")));
}
