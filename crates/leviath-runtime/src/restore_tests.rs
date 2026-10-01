use super::*;
use crate::runfile::reader_tests::{scripted_run, spec, write_run};

#[test]
fn a_run_without_a_run_file_is_left_to_the_older_path() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_for_resume(dir.path()).unwrap().is_none());
}

#[test]
fn a_run_file_that_cannot_be_read_is_an_error_not_a_fall_back() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(leviath_core::files::RUN_FILE), b"junk").unwrap();
    let err = read_for_resume(dir.path()).unwrap_err();
    assert!(err.to_string().contains("not a run file"));
    // A file whose spec names code it does not hold cannot be bound.
    let path = dir.path().join(leviath_core::files::RUN_FILE);
    let states = scripted_run(1);
    crate::runfile::RunFileWriter::create(
        &path,
        &spec(),
        &Default::default(),
        &states[0],
        Default::default(),
    )
    .unwrap();
    assert!(read_for_resume(dir.path()).is_err());
    // Nor can one with no state to start from.
    let mut bytes = crate::runfile::codec::header(crate::runfile::fingerprint());
    bytes.extend(
        crate::runfile::codec::encode(crate::runfile::codec::FrameKind::Spec, &spec()).unwrap(),
    );
    std::fs::write(&path, bytes).unwrap();
    assert!(read_for_resume(dir.path()).is_err());
}

#[test]
fn a_run_comes_back_from_its_file_at_its_last_step() {
    let dir = tempfile::tempdir().unwrap();
    let states = scripted_run(4);
    let path = dir.path().join(leviath_core::files::RUN_FILE);
    let written = write_run(&path, &states, Default::default());
    let run = read_for_resume(dir.path()).unwrap().expect("a run file");
    assert_eq!(run.state, *written.state());
    assert_eq!(*run.spec, spec());
    assert_eq!(run.code, crate::runfile::reader_tests::code());
    let mut world = World::new();
    let entity = resume(&mut world, run, crate::spec::env::Bindings::new());
    let placed = world.get::<crate::insert::RunSpecC>(entity).unwrap();
    assert_eq!(placed.0.run_id, spec().run_id);
}
