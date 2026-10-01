//! Reader tests, the run file's error messages, and the fixtures the other
//! run-file tests share: a spec, a scripted run's states, and a frame builder
//! for bytes the writer would never produce.

use std::path::Path;

use leviath_core::JsonDoc;

use super::codec::{self, CodecError, FrameKind};
use super::error::{RunFileError, RunFileErrorKind};
use super::reader::RunFileReader;
use super::writer::{CheckpointPolicy, RunFileWriter};
use crate::spec::env::CodeFiles;
use crate::spec::names::{Digest, RegionName, StageName};
use crate::spec::run_spec::RunSpec;
use crate::state::context::{
    BlobState, ContextState, EntryKind, EntryMeta, EntryState, PartBody, PartState, RegionState,
    ToolCallState,
};
use crate::state::{RunEvent, RunState, RunStatus, StageRecord, StageStatus, StateDelta};

pub(crate) fn spec() -> RunSpec {
    crate::spec::run_spec::tests::spec()
}

pub(crate) fn code() -> CodeFiles {
    [(Digest::of(b"code"), b"code".to_vec())].into()
}

pub(crate) fn entry(text: &str, kind: EntryKind) -> EntryState {
    EntryState {
        text: text.into(),
        parts: vec![],
        tokens: 3,
        timestamp: 10,
        kind,
        meta: EntryMeta::None,
        key: None,
        reasoning: None,
    }
}

fn region(name: &str) -> RegionState {
    RegionState {
        name: RegionName::new(name).unwrap(),
        max_tokens: 1000,
        current_tokens: 0,
        needs_message_compaction: false,
        taint: None,
        entries: vec![],
    }
}

/// A stored image part whose bytes are `bytes`.
pub(crate) fn image(bytes: &[u8]) -> PartState {
    PartState {
        mime_type: "image/png".into(),
        body: PartBody::Stored(BlobState {
            digest: Digest::of(bytes),
            size: bytes.len() as u64,
            width: Some(1),
            height: Some(1),
            duration_ms: None,
            tokens: 85,
            stand_in: "[image]".into(),
        }),
        name: Some("a.png".into()),
        deliver: None,
    }
}

pub(crate) fn initial() -> RunState {
    RunState::initial(
        StageName::new("plan").unwrap(),
        ContextState {
            regions: vec![region("system"), region("conversation")],
            hidden: vec![],
            max_tokens: 100_000,
        },
        true,
    )
}

/// The states of a scripted run of `turns` turns: model replies that call
/// tools, their results, a compaction every ten turns, an image now and
/// then, and a finish. Each is a whole state, as `inspect` would read it.
pub(crate) fn scripted_run(turns: usize) -> Vec<RunState> {
    let mut states = vec![initial()];
    let mut s = initial();
    s.status = RunStatus::Active;
    s.ledger.push(StageRecord {
        stage: StageName::new("plan").unwrap(),
        status: StageStatus::Active,
        entered: true,
        spend: Default::default(),
        models: vec![],
        visits: vec![],
        region_tokens: Default::default(),
        first_call_prompt_tokens: None,
        runaway_warned: false,
        output_cap_raised: false,
        started_at: Some(1),
        ended_at: None,
        clock: Default::default(),
    });
    for turn in 0..turns {
        let call = ToolCallState {
            id: format!("c{turn}"),
            name: "read_file".into(),
            args: JsonDoc::new(serde_json::json!({ "path": format!("src/f{turn}.rs") })),
            thought_signature: None,
        };
        let conv = &mut s.context.regions[1].entries;
        let mut reply = entry(
            &format!("Reading file {turn} to see how it fits together."),
            EntryKind::AssistantTurn(vec![call]),
        );
        reply.timestamp = turn as i64;
        conv.push(reply);
        let mut result = entry(
            &(0..20)
                .map(|i| format!("let v{turn}_{i} = {};\n", turn * 7919 + i * 104_729))
                .collect::<String>(),
            EntryKind::ToolResult {
                call_id: format!("c{turn}"),
                tool: "read_file".into(),
                is_error: false,
            },
        );
        if turn % 7 == 3 {
            result.parts.push(image(format!("png{turn}").as_bytes()));
        }
        conv.push(result);
        if turn % 10 == 9 {
            let summary = entry("Summary of the work so far.", EntryKind::Text);
            *conv = vec![summary];
        }
        s.cursor.iteration = turn as u32 + 1;
        s.progress.iterations = turn as u32 + 1;
        s.progress.total_tool_calls = turn as u32 + 1;
        s.totals.spend.prompt_tokens += 1000;
        s.totals.spend.completion_tokens += 50;
        s.totals.tool_calls += 1;
        s.ledger[0].spend.prompt_tokens += 1000;
        states.push(s.clone());
    }
    s.status = RunStatus::Complete;
    s.phase = crate::state::PipelinePhase::Done;
    states.push(s);
    states
}

/// Write `states` through a writer at `path`, returning the writer.
pub(crate) fn write_run(
    path: &Path,
    states: &[RunState],
    policy: CheckpointPolicy,
) -> RunFileWriter {
    let mut w = RunFileWriter::create(path, &spec(), &code(), &states[0], policy).unwrap();
    for (i, s) in states.iter().enumerate().skip(1) {
        w.record(
            s.clone(),
            i as i64,
            vec![RunEvent::Log(format!("step {i}"))],
        )
        .unwrap();
    }
    w
}

/// A frame around `body` as it is, with a good checksum.
pub(crate) fn raw_frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let len = (body.len() as u32).to_le_bytes();
    let mut crc = crc32fast::Hasher::new();
    crc.update(&[kind]);
    crc.update(body);
    [&[kind][..], &len, body, &crc.finalize().to_le_bytes(), &len].concat()
}

/// A frame whose payload is `raw` compressed, whatever `raw` is.
pub(crate) fn zstd_frame(kind: FrameKind, raw: &[u8]) -> Vec<u8> {
    raw_frame(kind as u8, &zstd::bulk::compress(raw, 3).unwrap())
}

fn file_of(frames: &[Vec<u8>]) -> Vec<u8> {
    let mut out = codec::header(super::fingerprint());
    for f in frames {
        out.extend(f);
    }
    out
}

pub(crate) fn spec_frame() -> Vec<u8> {
    codec::encode(FrameKind::Spec, &spec()).unwrap()
}

pub(crate) fn read(frames: &[Vec<u8>]) -> Result<RunFileReader, RunFileError> {
    RunFileReader::from_bytes(Path::new("r.lvr2"), file_of(frames))
}

fn kind(r: Result<RunFileReader, RunFileError>) -> RunFileErrorKind {
    r.unwrap_err().kind
}

#[test]
fn a_file_must_start_with_its_spec() {
    assert_eq!(kind(read(&[])), RunFileErrorKind::NoSpec);
    let delta_first = codec::encode(FrameKind::Delta, &1u64).unwrap();
    assert_eq!(kind(read(&[delta_first])), RunFileErrorKind::NoSpec);
    let not_a_spec = codec::encode(FrameKind::Spec, &"no").unwrap();
    assert!(matches!(
        kind(read(&[not_a_spec])),
        RunFileErrorKind::Codec(CodecError::Decode(_))
    ));
    let twice = [spec_frame(), spec_frame()];
    assert!(matches!(
        kind(read(&twice)),
        RunFileErrorKind::Codec(CodecError::Corrupt(_))
    ));
}

#[test]
fn a_file_from_another_build_is_refused_by_name() {
    let mut bytes = codec::header(&[9; 32]);
    bytes.extend(spec_frame());
    let err = RunFileReader::from_bytes(Path::new("old.lvr2"), bytes).unwrap_err();
    assert!(matches!(
        err.kind,
        RunFileErrorKind::Codec(CodecError::Fingerprint { .. })
    ));
    assert!(err.to_string().contains("old.lvr2"));
    assert!(err.to_string().contains("different run types"));
}

#[test]
fn frames_whose_heads_do_not_decode_are_refused() {
    let not_zstd = raw_frame(FrameKind::State as u8, b"plainly not zstd");
    assert!(matches!(
        kind(read(&[spec_frame(), not_zstd])),
        RunFileErrorKind::Codec(CodecError::Decode(_))
    ));
    let bad_varint = zstd_frame(FrameKind::Delta, &[0xff; 12]);
    assert!(matches!(
        kind(read(&[spec_frame(), bad_varint])),
        RunFileErrorKind::Codec(CodecError::Decode(_))
    ));
    let bad_digest = codec::encode(FrameKind::Blob, &("not a digest", 1u8)).unwrap();
    assert!(matches!(
        kind(read(&[spec_frame(), bad_digest])),
        RunFileErrorKind::Codec(CodecError::Decode(_))
    ));
}

#[test]
fn a_state_or_delta_that_does_not_decode_is_named_when_it_is_read() {
    // Each starts with a good `seq`, so it indexes, and is nothing after it.
    let state = codec::encode(FrameKind::State, &0u64).unwrap();
    let r = read(&[spec_frame(), state]).unwrap();
    assert!(matches!(
        r.state_at(0).unwrap_err().kind,
        RunFileErrorKind::Codec(CodecError::Decode(_))
    ));
    let good = codec::encode(FrameKind::State, &initial()).unwrap();
    let delta = codec::encode(FrameKind::Delta, &1u64).unwrap();
    let r = read(&[spec_frame(), good, delta]).unwrap();
    assert!(matches!(
        r.latest_state().unwrap_err().kind,
        RunFileErrorKind::Codec(CodecError::Decode(_))
    ));
    assert!(r.deltas(0, 5).is_err());
}

fn delta(seq: u64) -> Vec<u8> {
    let d = StateDelta {
        seq,
        at: 0,
        changes: vec![],
        events: vec![RunEvent::Log(format!("{seq}"))],
    };
    codec::encode(FrameKind::Delta, &d).unwrap()
}

#[test]
fn a_gap_in_the_steps_is_named_rather_than_skipped() {
    let state = codec::encode(FrameKind::State, &initial()).unwrap();
    let r = read(&[spec_frame(), state, delta(1), delta(3)]).unwrap();
    assert_eq!(r.last_seq(), 3);
    assert_eq!(r.state_at(1).unwrap().seq, 1);
    assert_eq!(
        r.latest_state().unwrap_err().kind,
        RunFileErrorKind::SeqGap {
            expected: 2,
            found: 3
        }
    );
    assert_eq!(
        r.state_at(4).unwrap_err().kind,
        RunFileErrorKind::NoSuchStep { seq: 4, last: 3 }
    );
    assert_eq!(r.deltas(2, 3).unwrap().len(), 1);
}

#[test]
fn a_file_with_no_checkpoint_opens_but_has_no_state() {
    let r = read(&[spec_frame(), delta(1)]).unwrap();
    assert_eq!(r.last_checkpoint(), (0, 0));
    assert_eq!(r.checkpoints(), 0);
    assert_eq!(r.state_at(1).unwrap_err().kind, RunFileErrorKind::NoState);
    assert_eq!(r.spec(), &spec());
    assert!(!r.is_empty());
    assert_eq!(r.path(), Path::new("r.lvr2"));
    assert_eq!(r.cut_bytes(), 0);
    assert!(r.owners().unwrap().is_empty());
    // The spec names code the file does not hold.
    assert_eq!(
        r.code_files().unwrap_err().kind,
        RunFileErrorKind::MissingCode(Digest::of(b"code"))
    );
    assert_eq!(r.code(&Digest::of(b"code")).unwrap(), None);
    assert_eq!(r.blob(&Digest::of(b"x")).unwrap(), None);
}

#[test]
fn opening_a_missing_file_says_which() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gone.lvr2");
    let err = RunFileReader::open(&path).unwrap_err();
    assert!(matches!(err.kind, RunFileErrorKind::Io(_)));
    assert_eq!(err.path, path);
    assert!(matches!(
        super::reader::truncate(&path, 0).unwrap_err().kind,
        RunFileErrorKind::Io(_)
    ));
}

#[test]
fn every_step_of_a_written_run_reads_back_as_the_state_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run.lvr2");
    let states = scripted_run(25);
    let policy = CheckpointPolicy {
        every: 4,
        size_ratio: 100.0,
    };
    let w = write_run(&path, &states, policy);
    let r = RunFileReader::open(&path).unwrap();
    assert_eq!(r.len(), w.len());
    assert_eq!(r.code_files().unwrap(), code());
    assert_eq!(r.code_digests().count(), 1);
    assert_eq!(r.last_seq(), states.len() as u64 - 1);
    for (seq, expected) in states.iter().enumerate() {
        let mut expected = expected.clone();
        expected.seq = seq as u64;
        assert_eq!(r.state_at(seq as u64).unwrap(), expected);
    }
    assert_eq!(r.latest_state().unwrap(), *w.state());
    // The deltas between any two steps replay one onto the other.
    let mut replayed = r.state_at(5).unwrap();
    for d in r.deltas(6, 12).unwrap() {
        d.apply(&mut replayed);
    }
    assert_eq!(replayed, r.state_at(12).unwrap());
    // Every image the run showed was stored once.
    let images: Vec<_> = r.blob_digests().collect();
    assert!(images.is_empty(), "the writer alone stores no blobs");
}

#[test]
fn a_torn_tail_is_cut_off_the_file_and_the_rest_kept() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run.lvr2");
    let states = scripted_run(3);
    let w = write_run(&path, &states, CheckpointPolicy::default());
    let whole = w.len();
    drop(w);
    let mut bytes = std::fs::read(&path).unwrap();
    let next = codec::encode(
        FrameKind::Delta,
        &StateDelta {
            seq: 99,
            at: 0,
            changes: vec![],
            events: vec![],
        },
    )
    .unwrap();
    bytes.extend(&next[..next.len() - 2]);
    std::fs::write(&path, &bytes).unwrap();
    let r = RunFileReader::open(&path).unwrap();
    assert_eq!(r.cut_bytes(), next.len() - 2);
    assert_eq!(r.len(), whole);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), whole);
    assert_eq!(r.latest_state().unwrap().seq, states.len() as u64 - 1);
    // And a writer carries on from it.
    let mut w = RunFileWriter::open(&path, CheckpointPolicy::default()).unwrap();
    let mut more = states.last().unwrap().clone();
    more.title = Some("after the tear".into());
    assert_eq!(
        w.record(more, 1, vec![]).unwrap(),
        Some(states.len() as u64)
    );
}

#[test]
fn every_error_names_its_file_and_what_is_wrong() {
    let p = Path::new("runs/r1/run.lvr2");
    let cases = [
        (
            RunFileError::io(p, &std::io::Error::other("disk gone")),
            "cannot be read or written: disk gone",
        ),
        (
            RunFileError::codec(p, CodecError::NotARunFile),
            "not a run file",
        ),
        (
            RunFileError::new(p, RunFileErrorKind::NoSpec),
            "does not start with the run's spec",
        ),
        (
            RunFileError::new(p, RunFileErrorKind::NoState),
            "no state checkpoint",
        ),
        (
            RunFileError::new(
                p,
                RunFileErrorKind::SeqGap {
                    expected: 3,
                    found: 5,
                },
            ),
            "skips from step 2 to step 5",
        ),
        (
            RunFileError::new(p, RunFileErrorKind::NoSuchStep { seq: 9, last: 4 }),
            "has no step 9; its last step is 4",
        ),
        (
            RunFileError::new(p, RunFileErrorKind::MissingCode(Digest::of(b"x"))),
            "does not hold the code",
        ),
    ];
    for (err, says) in cases {
        let text = err.to_string();
        assert!(text.starts_with("run file runs/r1/run.lvr2: "), "{text}");
        assert!(text.contains(says), "{text}");
    }
    let err: Box<dyn std::error::Error> = Box::new(RunFileError::new(p, RunFileErrorKind::NoSpec));
    assert!(err.source().is_none());
}

/// The shape the legacy converter writes: code and blob payloads as a
/// `(digest, bytes)` tuple, the first state at step 0 straight after them,
/// a delta per step, and a closing state.
#[test]
fn a_converted_run_reads_like_a_written_one() {
    let states = scripted_run(3);
    let code_digest = Digest::of(b"code");
    let blob_digest = Digest::of(b"png");
    let mut frames = vec![
        spec_frame(),
        codec::encode(FrameKind::Code, &(code_digest.clone(), b"code".to_vec())).unwrap(),
        codec::encode(FrameKind::Blob, &(blob_digest.clone(), b"png".to_vec())).unwrap(),
        codec::encode(FrameKind::State, &states[0]).unwrap(),
    ];
    let mut at = states[0].clone();
    for (i, next) in states.iter().enumerate().skip(1) {
        let d = StateDelta::between(&at, next, i as i64, vec![]);
        at = next.clone();
        at.seq = d.seq;
        frames.push(codec::encode(FrameKind::Delta, &d).unwrap());
    }
    frames.push(codec::encode(FrameKind::State, &at).unwrap());
    let r = read(&frames).unwrap();
    assert_eq!(r.code(&code_digest).unwrap(), Some(b"code".to_vec()));
    assert_eq!(r.blob(&blob_digest).unwrap(), Some(b"png".to_vec()));
    assert_eq!(r.latest_state().unwrap(), at);
    assert_eq!(r.state_at(0).unwrap(), states[0]);
    assert_eq!(r.checkpoints(), 2);
}

#[test]
fn code_or_a_blob_whose_frame_holds_only_its_digest_is_named() {
    let only_digest = |kind| codec::encode(kind, &Digest::of(b"code")).unwrap();
    let r = read(&[
        spec_frame(),
        only_digest(FrameKind::Code),
        only_digest(FrameKind::Blob),
    ])
    .unwrap();
    let decode = |e: RunFileError| matches!(e.kind, RunFileErrorKind::Codec(CodecError::Decode(_)));
    assert!(decode(r.code(&Digest::of(b"code")).unwrap_err()));
    assert!(decode(r.code_files().unwrap_err()));
    assert!(decode(r.blob(&Digest::of(b"code")).unwrap_err()));
}

/// Read-only, so the torn tail cannot be cut off it.
fn read_only(path: &Path, on: bool) {
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_readonly(on);
    std::fs::set_permissions(path, perms).unwrap();
}

#[test]
fn a_torn_tail_that_cannot_be_cut_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run.lvr2");
    write_run(&path, &scripted_run(1), CheckpointPolicy::default());
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend(b"torn");
    std::fs::write(&path, &bytes).unwrap();
    read_only(&path, true);
    let err = RunFileReader::open(&path).unwrap_err();
    assert!(matches!(err.kind, RunFileErrorKind::Io(_)));
    // And a writer cannot append to a file it may only read.
    read_only(&path, false);
    std::fs::write(&path, &bytes[..bytes.len() - 4]).unwrap();
    read_only(&path, true);
    let err = RunFileWriter::open(&path, CheckpointPolicy::default()).unwrap_err();
    assert!(matches!(err.kind, RunFileErrorKind::Io(_)));
    // Writable again, so the directory can be cleaned up on every platform.
    read_only(&path, false);
}
