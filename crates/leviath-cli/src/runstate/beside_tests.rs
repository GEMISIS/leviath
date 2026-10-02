use super::*;
use crate::runstate::{create_run, run_dir, with_isolated_runs_dir};

/// A run's logs and answer are found through what its run file names: a
/// file the run file does not name is not read, even where a run would keep
/// it, and a file it names is read wherever it says.
#[test]
fn files_are_found_through_what_the_run_file_names() {
    with_isolated_runs_dir("beside-named", |_| {
        create_run(&crate::test_support::fixtures::run_meta("named")).unwrap();
        let dir = run_dir("named");
        std::fs::create_dir_all(dir.join("stages/0")).unwrap();
        std::fs::write(dir.join("stages/0/logs.log"), "unnamed\n").unwrap();
        assert_eq!(tail_stage_file(&dir, 0, StageFile::Logs, 100), "");
        assert_eq!(stage_file_path(&dir, 0, StageFile::Logs), None);
        assert_eq!(final_output_path(&dir), None);

        // Named somewhere else entirely, it is read from there.
        std::fs::create_dir_all(dir.join("legacy/stages/0")).unwrap();
        std::fs::write(dir.join("legacy/stages/0/logs.log"), "moved\n").unwrap();
        crate::runstate::fixtures_tests::name_files(&dir, |files| {
            files.set_stage_file(
                0,
                StageFile::Logs,
                FileRef::log("legacy/stages/0/logs.log", 6),
            );
        });
        assert_eq!(tail_stage_file(&dir, 0, StageFile::Logs, 100), "moved\n");
        assert_eq!(
            stage_file_path(&dir, 0, StageFile::Logs),
            Some(dir.join("legacy/stages/0/logs.log"))
        );
        assert_eq!(tail_stage_file(&dir, 1, StageFile::Logs, 100), "");
        assert_eq!(tail_stage_file(&dir, usize::MAX, StageFile::Logs, 100), "");
        assert_eq!(stage_file_path(&dir, usize::MAX, StageFile::Logs), None);
        // A directory with no run file names nothing.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(*named_files(empty.path()), RunFiles::default());
    });
}

/// A file that is not as the run file names it is still read (the log says
/// so); one that is gone reads as nothing; a path that leaves the run's
/// directory is never followed.
#[test]
fn a_file_not_as_named_is_read_and_said_and_one_outside_is_refused() {
    with_isolated_runs_dir("beside-changed", |_| {
        crate::test_support::with_tracing(|| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("final_output"), "edited").unwrap();
            let answer = FileRef::whole("final_output", b"original");
            assert_eq!(read_named(dir.path(), &answer).unwrap(), b"edited");

            std::fs::write(dir.path().join("logs.log"), "cut\n").unwrap();
            let log = FileRef::log("logs.log", 100);
            assert_eq!(tail_named(dir.path(), &log, 100), "cut\n");

            let gone = FileRef::log("gone.log", 0);
            assert_eq!(tail_named(dir.path(), &gone, 100), "");
            assert_eq!(read_named(dir.path(), &gone), None);

            let outside = FileRef::log("../elsewhere", 0);
            assert_eq!(tail_named(dir.path(), &outside, 100), "");
            assert_eq!(read_named(dir.path(), &outside), None);
        });
    });
}
