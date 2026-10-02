//! A run read from its ends must be the run read whole: the same state, the
//! same moment it last moved, and the same error for a file that does not
//! read.

use std::path::{Path, PathBuf};

use super::super::codec::{self, FrameKind};
use super::super::error::RunFileErrorKind;
use super::super::frames::BlobFrame;
use super::super::reader::{RunFileReader, read_file};
use super::super::reader_tests::{initial, scripted_run, spec, spec_frame, write_run};
use super::super::writer::CheckpointPolicy;
use super::{RunFileTail, from_ends, read_spec};
use crate::spec::names::Digest;
use crate::state::{RunEvent, RunState, StateDelta};

/// What the whole reader says about the file at `path`, and what reading it
/// from its ends says, which must agree.
fn both(path: &Path) -> Result<RunFileTail, RunFileErrorKind> {
    let whole = RunFileReader::read(path).and_then(|reader| RunFileTail::of(&reader));
    let ends = RunFileTail::read(path);
    match (whole, ends) {
        (Ok(whole), Ok(ends)) => {
            assert_eq!(whole, ends, "{}", path.display());
            Ok(ends)
        }
        (Err(whole), Err(ends)) => {
            assert_eq!(whole.kind, ends.kind, "{}", path.display());
            Err(ends.kind)
        }
        (whole, ends) => panic!("{}: whole {whole:?}, ends {ends:?}", path.display()),
    }
}

fn file_of(dir: &Path, name: &str, frames: &[Vec<u8>]) -> PathBuf {
    let mut bytes = codec::header(super::super::fingerprint());
    for frame in frames {
        bytes.extend(frame);
    }
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// `len` bytes that do not compress.
fn noise(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn delta(seq: u64, at: i64) -> Vec<u8> {
    let d = StateDelta {
        seq,
        at,
        changes: vec![],
        events: vec![RunEvent::Log(format!("{seq}"))],
    };
    codec::encode(FrameKind::Delta, &d).unwrap()
}

fn checkpoint(seq: u64) -> Vec<u8> {
    let mut state: RunState = initial();
    state.seq = seq;
    codec::encode(FrameKind::State, &state).unwrap()
}

fn blob() -> Vec<u8> {
    let frame = BlobFrame {
        digest: Digest::of(b"x"),
        bytes: b"x".to_vec(),
    };
    codec::encode(FrameKind::Blob, &frame).unwrap()
}

#[test]
fn a_written_run_reads_the_same_from_its_ends_at_every_step() {
    let dir = tempfile::tempdir().unwrap();
    let states = scripted_run(30);
    let policies = [
        CheckpointPolicy::default(),
        CheckpointPolicy {
            every: 1,
            size_ratio: 1e9,
        },
        CheckpointPolicy {
            every: 7,
            size_ratio: 1e9,
        },
    ];
    for (p, policy) in policies.into_iter().enumerate() {
        for len in 1..=states.len() {
            let path = dir.path().join(format!("{p}-{len}.lvr"));
            write_run(&path, &states[..len], policy);
            let tail = both(&path).unwrap();
            let mut want = states[len - 1].clone();
            want.seq = len as u64 - 1;
            assert_eq!(tail.state, want);
            // Read from its ends, not whole: that is what this is for.
            let bytes = std::fs::read(&path).unwrap();
            assert!(from_ends(&bytes).is_some(), "{p}-{len} was read whole");
        }
    }
}

/// A file bigger than what is read from its end at first is read from its
/// end all the same, reaching further back until the checkpoint is in what
/// was read, and comes out as the whole reader reads it.
#[test]
fn a_long_file_is_read_from_its_end_further_back_as_needed() {
    use crate::state::RunEvent;
    let dir = tempfile::tempdir().unwrap();
    let states = scripted_run(40);
    let filler = noise(300_000);
    // A finished run ends in a checkpoint; one still going ends in the
    // steps after its last one.
    for (name, steps) in [("finished", states.len()), ("going", 36)] {
        let path = dir.path().join(name);
        let policy = CheckpointPolicy {
            every: 10,
            size_ratio: 1e9,
        };
        let mut writer = super::super::writer::RunFileWriter::create(
            &path,
            &spec(),
            &super::super::reader_tests::code(),
            &states[0],
            policy,
        )
        .unwrap();
        writer.add_blob(&Digest::of(&filler), &filler).unwrap();
        for (i, state) in states.iter().enumerate().take(steps).skip(1) {
            writer
                .record(state.clone(), i as i64, vec![RunEvent::Log(format!("{i}"))])
                .unwrap();
        }
        drop(writer);
        let whole = RunFileReader::read(&path)
            .and_then(|r| RunFileTail::of(&r))
            .unwrap();
        // From one byte back, which is never enough: the window grows until
        // the checkpoint and the steps after it are in it.
        assert_eq!(RunFileTail::read_from_end(&path, 1).unwrap(), whole);
        assert_eq!(RunFileTail::read(&path).unwrap(), whole);
    }
}

#[test]
fn a_file_the_ends_do_not_settle_is_read_whole() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let cases: Vec<(&str, Vec<Vec<u8>>)> = vec![
        ("spec-only", vec![spec_frame()]),
        ("no-checkpoint", vec![spec_frame(), delta(1, 5)]),
        ("delta-first", vec![delta(1, 5)]),
        (
            "not-a-spec",
            vec![codec::encode(FrameKind::Spec, &"no").unwrap()],
        ),
        (
            "state-does-not-decode",
            vec![
                spec_frame(),
                codec::encode(FrameKind::State, &1u64).unwrap(),
            ],
        ),
        (
            "step-does-not-decode",
            vec![
                spec_frame(),
                checkpoint(0),
                codec::encode(FrameKind::Delta, &1u64).unwrap(),
            ],
        ),
        (
            "gap",
            vec![spec_frame(), checkpoint(0), delta(1, 5), delta(3, 6)],
        ),
        (
            "blob-last",
            vec![spec_frame(), checkpoint(0), delta(1, 5), blob()],
        ),
        (
            "checkpoint-after-a-blob",
            vec![
                spec_frame(),
                checkpoint(0),
                delta(1, 5),
                blob(),
                checkpoint(1),
            ],
        ),
        (
            "checkpoint-after-another-step",
            vec![spec_frame(), checkpoint(0), delta(1, 5), checkpoint(2)],
        ),
        (
            "checkpoint-step-does-not-decode",
            vec![
                spec_frame(),
                checkpoint(0),
                codec::encode(FrameKind::Delta, &1u64).unwrap(),
                checkpoint(1),
            ],
        ),
        ("first-checkpoint", vec![spec_frame(), checkpoint(0)]),
        (
            "checkpoint-after-its-step",
            vec![spec_frame(), checkpoint(0), delta(1, 5), checkpoint(1)],
        ),
    ];
    let mut read = Vec::new();
    for (name, frames) in &cases {
        let path = file_of(d, name, frames);
        read.push((*name, both(&path).map(|tail| tail.updated_at)));
    }
    let created = spec().created_at;
    assert_eq!(
        read,
        vec![
            ("spec-only", Err(RunFileErrorKind::NoState)),
            ("no-checkpoint", Err(RunFileErrorKind::NoState)),
            ("delta-first", Err(RunFileErrorKind::NoSpec)),
            ("not-a-spec", read[3].1.clone()),
            ("state-does-not-decode", read[4].1.clone()),
            ("step-does-not-decode", read[5].1.clone()),
            (
                "gap",
                Err(RunFileErrorKind::SeqGap {
                    expected: 2,
                    found: 3
                })
            ),
            ("blob-last", Ok(5)),
            ("checkpoint-after-a-blob", Ok(5)),
            ("checkpoint-after-another-step", Ok(created)),
            ("checkpoint-step-does-not-decode", Ok(created)),
            ("first-checkpoint", Ok(created)),
            ("checkpoint-after-its-step", Ok(5)),
        ]
    );
    for i in [3, 4, 5] {
        assert!(matches!(
            read[i].1,
            Err(RunFileErrorKind::Codec(codec::CodecError::Decode(_)))
        ));
    }

    // A torn tail: the whole reader drops it, and so the answer is the run
    // before it.
    let path = d.join("torn.lvr");
    write_run(&path, &scripted_run(3), CheckpointPolicy::default());
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(&[4, 200, 0, 0, 0, 1, 2, 3]);
    std::fs::write(&path, &bytes).unwrap();
    assert!(from_ends(&bytes).is_none());
    both(&path).unwrap();
    // And nothing was written to the file to read it.
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn a_file_that_is_not_a_run_file_is_refused_from_its_header() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old.lvr");
    let mut bytes = b"LVR1".to_vec();
    bytes.resize(1 << 20, 7);
    std::fs::write(&old, &bytes).unwrap();
    assert_eq!(read_file(&old).unwrap().len(), codec::HEADER_LEN);
    let refused = RunFileErrorKind::Codec(codec::CodecError::NotARunFile);
    assert_eq!(both(&old).unwrap_err(), refused);
    assert_eq!(read_spec(&old).unwrap_err().kind, refused);

    let other_build = dir.path().join("other.lvr");
    let mut bytes = codec::header(&[9; 32]);
    bytes.extend(spec_frame());
    std::fs::write(&other_build, &bytes).unwrap();
    assert!(matches!(
        both(&other_build).unwrap_err(),
        RunFileErrorKind::Codec(codec::CodecError::Fingerprint { .. })
    ));

    let gone = dir.path().join("gone.lvr");
    assert!(matches!(both(&gone).unwrap_err(), RunFileErrorKind::Io(_)));
    assert!(matches!(
        read_spec(&gone).unwrap_err().kind,
        RunFileErrorKind::Io(_)
    ));
}

/// A spec bigger than the front of the file that is read for it is read
/// with the whole file, by both readers.
#[test]
fn a_spec_bigger_than_the_front_read_is_read_whole() {
    use crate::spec::names::RegionName;
    use crate::spec::run_spec::SeededContent;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big-spec.lvr");
    let mut big = spec();
    let text: String = noise(500_000)
        .iter()
        .map(|b| char::from(b'a' + b % 26))
        .collect();
    big.seeded.insert(
        RegionName::new("big").unwrap(),
        SeededContent {
            text,
            parts: Vec::new(),
        },
    );
    let mut writer = super::super::writer::RunFileWriter::create(
        &path,
        &big,
        &super::super::reader_tests::code(),
        &initial(),
        CheckpointPolicy::default(),
    )
    .unwrap();
    writer
        .record(initial(), 7, vec![RunEvent::Log("one".into())])
        .unwrap();
    drop(writer);
    assert!(
        std::fs::metadata(&path).unwrap().len() > 2 * super::HEAD_READ,
        "the spec is big"
    );
    assert_eq!(read_spec(&path).unwrap(), big);
    let tail = RunFileTail::read_from_end(&path, 1).unwrap();
    assert_eq!(tail.spec, big);
    assert_eq!(tail.updated_at, 7);
}

#[test]
fn the_spec_is_read_from_the_front_of_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run.lvr");
    write_run(&path, &scripted_run(5), CheckpointPolicy::default());
    assert_eq!(read_spec(&path).unwrap(), spec());
    // A first frame that is not a spec is left to the whole reader to name.
    let no_spec = file_of(dir.path(), "no-spec.lvr", &[delta(1, 5)]);
    assert_eq!(
        read_spec(&no_spec).unwrap_err().kind,
        RunFileErrorKind::NoSpec
    );
}
