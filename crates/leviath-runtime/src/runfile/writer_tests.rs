use super::*;
use crate::runfile::reader_tests::{code, initial, scripted_run, spec, write_run};

fn temp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run.lvr2");
    (dir, path)
}

fn changed(from: &RunState, title: &str) -> RunState {
    RunState {
        title: Some(title.into()),
        ..from.clone()
    }
}

#[test]
fn a_new_file_holds_the_spec_its_code_and_the_first_state() {
    let (_dir, path) = temp();
    let w = RunFileWriter::create(
        &path,
        &spec(),
        &code(),
        &initial(),
        CheckpointPolicy::default(),
    )
    .unwrap();
    assert_eq!(w.path(), path);
    assert!(!w.is_empty());
    assert_eq!(w.seq(), 0);
    assert!(w.has_code(&Digest::of(b"code")));
    assert!(!w.checkpoint_due());
    let r = RunFileReader::open(&path).unwrap();
    assert_eq!(r.spec(), &spec());
    assert_eq!(
        r.code(&Digest::of(b"code")).unwrap(),
        Some(b"code".to_vec())
    );
    assert_eq!(r.latest_state().unwrap(), initial());
    assert_eq!(r.checkpoints(), 1);
}

#[test]
fn the_file_is_private_to_its_owner() {
    let (_dir, path) = temp();
    RunFileWriter::create(
        &path,
        &spec(),
        &code(),
        &initial(),
        CheckpointPolicy::default(),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert!(path.exists());
}

#[test]
fn a_file_that_cannot_be_made_or_opened_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let nowhere = dir.path().join("no-such-dir").join("run.lvr2");
    let err = RunFileWriter::create(&nowhere, &spec(), &code(), &initial(), Default::default())
        .unwrap_err();
    assert!(matches!(err.kind, RunFileErrorKind::Io(_)));
    assert!(RunFileWriter::open(&nowhere, Default::default()).is_err());
    // A file with a spec and no checkpoint has nothing to carry on from.
    let (_d, path) = temp();
    let mut bytes = codec::header(crate::runfile::fingerprint());
    bytes.extend(codec::encode(FrameKind::Spec, &spec()).unwrap());
    std::fs::write(&path, bytes).unwrap();
    assert_eq!(
        RunFileWriter::open(&path, Default::default())
            .unwrap_err()
            .kind,
        RunFileErrorKind::NoState
    );
}

#[test]
fn steps_must_follow_one_another() {
    let (_dir, path) = temp();
    let mut w =
        RunFileWriter::create(&path, &spec(), &code(), &initial(), Default::default()).unwrap();
    let next = changed(&initial(), "t");
    let mut delta = StateDelta::between(&initial(), &next, 1, vec![]);
    delta.seq = 5;
    assert_eq!(
        w.append_delta(&delta).unwrap_err().kind,
        RunFileErrorKind::SeqGap {
            expected: 1,
            found: 5
        }
    );
    delta.seq = 1;
    w.append_delta(&delta).unwrap();
    assert_eq!(w.state().title.as_deref(), Some("t"));
    assert_eq!(w.seq(), 1);
    // A checkpoint taken by hand is where the next read starts.
    w.checkpoint(&w.state().clone()).unwrap();
    let r = RunFileReader::open(&path).unwrap();
    assert_eq!(r.checkpoints(), 2);
    assert_eq!(r.last_checkpoint().0, 1);
}

#[test]
fn nothing_changed_writes_nothing() {
    let (_dir, path) = temp();
    let mut w =
        RunFileWriter::create(&path, &spec(), &code(), &initial(), Default::default()).unwrap();
    let before = w.len();
    assert_eq!(w.record(initial(), 1, vec![]).unwrap(), None);
    assert_eq!(w.len(), before);
    // An event alone is a step.
    let events = vec![RunEvent::Log("hello".into())];
    assert_eq!(w.record(initial(), 2, events).unwrap(), Some(1));
}

#[test]
fn checkpoints_come_every_so_many_steps() {
    let (_dir, path) = temp();
    let states = scripted_run(20);
    let policy = CheckpointPolicy {
        every: 5,
        size_ratio: 1000.0,
    };
    write_run(&path, &states, policy);
    let r = RunFileReader::open(&path).unwrap();
    // The first, one after every fifth step, and one at the finish.
    let steps = states.len() as u32 - 1;
    assert_eq!(
        r.checkpoints() as u32,
        1 + steps / 5 + u32::from(!steps.is_multiple_of(5))
    );
}

#[test]
fn checkpoints_come_when_the_deltas_outgrow_the_last_one() {
    let (_dir, path) = temp();
    let states = scripted_run(20);
    let by_count_only = CheckpointPolicy {
        every: 1000,
        size_ratio: 1000.0,
    };
    write_run(&path, &states, by_count_only);
    let rare = RunFileReader::open(&path).unwrap().checkpoints();
    let by_size = CheckpointPolicy {
        every: 1000,
        size_ratio: 0.2,
    };
    write_run(&path, &states, by_size);
    let sized = RunFileReader::open(&path).unwrap().checkpoints();
    assert_eq!(rare, 2);
    assert!(sized > rare);
}

#[test]
fn a_reopened_writer_carries_on_counting_toward_its_next_checkpoint() {
    let (_dir, path) = temp();
    let states = scripted_run(3);
    let policy = CheckpointPolicy {
        every: 3,
        size_ratio: 1000.0,
    };
    let mut w = RunFileWriter::create(&path, &spec(), &code(), &states[0], policy).unwrap();
    w.record(states[1].clone(), 1, vec![]).unwrap();
    w.record(states[2].clone(), 2, vec![]).unwrap();
    drop(w);
    let mut w = RunFileWriter::open(&path, policy).unwrap();
    assert_eq!(w.seq(), 2);
    assert!(!w.checkpoint_due());
    w.record(states[3].clone(), 3, vec![]).unwrap();
    assert_eq!(RunFileReader::open(&path).unwrap().checkpoints(), 2);
}

#[test]
fn blobs_and_code_are_stored_once() {
    let (_dir, path) = temp();
    let mut w = RunFileWriter::create(
        &path,
        &spec(),
        &CodeFiles::new(),
        &initial(),
        Default::default(),
    )
    .unwrap();
    let d = Digest::of(b"png");
    assert!(w.add_blob(&d, b"png").unwrap());
    assert!(!w.add_blob(&d, b"png").unwrap());
    assert!(w.has_blob(&d));
    let c = Digest::of(b"code");
    assert!(w.add_code(&c, b"code").unwrap());
    assert!(!w.add_code(&c, b"code").unwrap());
    let r = RunFileReader::open(&path).unwrap();
    assert_eq!(r.blob(&d).unwrap(), Some(b"png".to_vec()));
    assert_eq!(r.code_files().unwrap(), code());
    // A reopened writer knows what the file already holds.
    let w = RunFileWriter::open(&path, Default::default()).unwrap();
    assert!(w.has_blob(&d));
    assert!(w.has_code(&c));
}

#[test]
fn a_new_owner_is_written_with_the_next_step_or_at_once() {
    let (_dir, path) = temp();
    let mut w =
        RunFileWriter::create(&path, &spec(), &code(), &initial(), Default::default()).unwrap();
    let owner = |at| OwnerFrame {
        machine_id: "m".into(),
        world_id: "w".into(),
        at,
    };
    w.set_owner(&owner(1)).unwrap();
    w.owner_on_next_step(owner(2));
    assert_eq!(
        RunFileReader::open(&path).unwrap().owners().unwrap(),
        vec![owner(1)]
    );
    w.record(changed(&initial(), "x"), 3, vec![]).unwrap();
    assert_eq!(
        RunFileReader::open(&path).unwrap().owners().unwrap(),
        vec![owner(1), owner(2)]
    );
}

#[test]
fn a_failed_write_leaves_the_file_as_it_was() {
    let (_dir, path) = temp();
    let mut w =
        RunFileWriter::create(&path, &spec(), &code(), &initial(), Default::default()).unwrap();
    let before = w.len();
    break_writes(&mut w);
    assert!(matches!(
        w.add_blob(&Digest::of(b"x"), b"x").unwrap_err().kind,
        RunFileErrorKind::Io(_)
    ));
    assert!(w.add_code(&Digest::of(b"y"), b"y").is_err());
    assert!(w.record(changed(&initial(), "t"), 1, vec![]).is_err());
    assert!(w.checkpoint(&initial()).is_err());
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
    assert_eq!(w.seq(), 0);
    let r = RunFileReader::open(&path).unwrap();
    assert_eq!(r.latest_state().unwrap(), initial());
}

#[test]
fn a_finished_run_ends_on_a_checkpoint() {
    let (_dir, path) = temp();
    let states = scripted_run(2);
    let policy = CheckpointPolicy {
        every: 1000,
        size_ratio: 1000.0,
    };
    write_run(&path, &states, policy);
    let r = RunFileReader::open(&path).unwrap();
    assert_eq!(r.last_checkpoint().0, r.last_seq());
    for status in [RunStatus::Error("x".into()), RunStatus::Cancelled] {
        let (_d, p) = temp();
        let mut w = RunFileWriter::create(&p, &spec(), &code(), &initial(), policy).unwrap();
        let done = RunState {
            status,
            ..initial()
        };
        w.record(done, 1, vec![]).unwrap();
        assert_eq!(RunFileReader::open(&p).unwrap().checkpoints(), 2);
    }
}

/// Not a check: prints what the default policy costs on a scripted run, next
/// to the same run's deltas alone and its checkpoints alone. Run with
/// `--nocapture` to see it.
#[test]
fn measure_the_default_checkpoint_policy() {
    let (_dir, path) = temp();
    let states = scripted_run(120);
    let size = |policy| {
        write_run(&path, &states, policy);
        let r = RunFileReader::open(&path).unwrap();
        (std::fs::metadata(&path).unwrap().len(), r.checkpoints())
    };
    let deltas_only = size(CheckpointPolicy {
        every: u32::MAX,
        size_ratio: f64::MAX,
    });
    let default = size(CheckpointPolicy::default());
    for (every, size_ratio) in [
        (16, 0.5),
        (32, 0.5),
        (32, 1.0),
        (32, 2.0),
        (64, 2.0),
        (64, 4.0),
    ] {
        let got = size(CheckpointPolicy { every, size_ratio });
        println!("every {every}, ratio {size_ratio}: {got:?}");
    }
    let every_step = size(CheckpointPolicy {
        every: 1,
        size_ratio: 0.0,
    });
    println!("deltas only: {deltas_only:?}, default: {default:?}, every step: {every_step:?}");
    assert!(deltas_only.0 <= default.0 && default.0 <= every_step.0);
}
