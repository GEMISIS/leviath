use super::*;
use crate::runfile::reader_tests::{scripted_run, spec, write_run};
use crate::state::{PipelinePhase, RunStatus};

#[test]
fn a_directory_without_a_run_file_has_nothing_to_resume() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_for_resume(dir.path()).unwrap().is_none());
}

#[test]
fn a_run_file_that_cannot_be_read_is_an_error() {
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

fn resumable(depth: u8, created_at: i64, status: RunStatus, phase: PipelinePhase) -> Resumable {
    let mut spec = spec();
    spec.placement.depth = depth;
    spec.created_at = created_at;
    let mut state = scripted_run(1).remove(0);
    state.status = status;
    state.phase = phase;
    Resumable {
        spec: std::sync::Arc::new(spec),
        state,
        code: Default::default(),
    }
}

#[test]
fn every_status_and_phase_is_classified() {
    use RestorePriority::{Active, Blocked};
    let cases = [
        (RunStatus::Complete, PipelinePhase::Done, None),
        (RunStatus::Error("x".into()), PipelinePhase::Done, None),
        (RunStatus::Cancelled, PipelinePhase::Done, None),
        (RunStatus::Paused, PipelinePhase::Paused, Some(Blocked)),
        (
            RunStatus::Waiting,
            PipelinePhase::AwaitingPerson,
            Some(Blocked),
        ),
        (RunStatus::Active, PipelinePhase::FanOut, Some(Blocked)),
        (
            RunStatus::Active,
            PipelinePhase::AwaitingPerson,
            Some(Blocked),
        ),
        (RunStatus::Active, PipelinePhase::Paused, Some(Blocked)),
        (
            RunStatus::Active,
            PipelinePhase::WaitingForChildren,
            Some(Blocked),
        ),
        (
            RunStatus::Active,
            PipelinePhase::AwaitingTools,
            Some(Active),
        ),
        (RunStatus::Idle, PipelinePhase::ReadyToInfer, Some(Active)),
    ];
    for (status, phase, want) in cases {
        let run = resumable(0, 0, status.clone(), phase.clone());
        assert_eq!(classify(&run.state), want, "{status:?} / {phase:?}");
    }
}

#[test]
fn children_come_back_first_then_active_runs_then_the_newest() {
    let runs = vec![
        resumable(0, 1, RunStatus::Paused, PipelinePhase::Paused),
        resumable(0, 2, RunStatus::Active, PipelinePhase::ReadyToInfer),
        resumable(0, 3, RunStatus::Complete, PipelinePhase::Done),
        resumable(1, 0, RunStatus::Waiting, PipelinePhase::AwaitingPerson),
        resumable(0, 5, RunStatus::Active, PipelinePhase::AwaitingTools),
    ];
    let order: Vec<(u8, i64)> = triage(runs, |r| r)
        .iter()
        .map(|r| (r.spec.placement.depth, r.spec.created_at))
        .collect();
    assert_eq!(order, vec![(1, 0), (0, 5), (0, 2), (0, 1)]);
}
