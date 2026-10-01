//! Runs read off their run files: the listings, the stat cache over them,
//! and the forced cancel.

use super::*;
use crate::config::Config;
use crate::daemon::starter::testing::{manifest_in, run_on_disk};
use std::sync::Arc;

/// A run of the coder blueprint recorded under `runs`. Returns its id.
fn recorded(runs: &Path) -> String {
    let agent = tempfile::tempdir().unwrap().keep();
    let manifest = manifest_in(&agent, &crate::test_support::inline_coder_manifest());
    let mut registry = leviath_runtime::ProviderRegistry::new();
    registry.register(
        "anthropic".to_string(),
        Arc::new(crate::test_support::FakeProvider::new().context_window(100_000)),
    );
    run_on_disk(Config::default(), registry, runs, &manifest)
}

#[tokio::test]
async fn a_run_is_listed_from_its_run_file() {
    let runs = tempfile::tempdir().unwrap();
    let run_id = recorded(runs.path());
    let meta = read_meta_from(&runs.path().join(&run_id)).expect("the run file reads");
    assert_eq!(meta.run_id, run_id);
    assert_eq!(meta.agent_name, "coder");
    assert!(!is_terminal_status(&meta.status));

    let listed = list_runs_in_dir(runs.path().to_path_buf());
    assert_eq!(
        listed.iter().map(|m| m.run_id.clone()).collect::<Vec<_>>(),
        std::slice::from_ref(&run_id)
    );
    assert_eq!(
        listing_file(&runs.path().join(&run_id)),
        runs.path()
            .join(&run_id)
            .join(leviath_core::files::RUN_FILE)
    );
    assert_eq!(
        listing_file(&runs.path().join("older")),
        runs.path()
            .join("older")
            .join(leviath_core::files::META_FILE)
    );
}

/// The poller's cache reads a run file once per change, and drops a run
/// whose file is gone.
#[tokio::test]
async fn the_cache_reads_a_run_file_once_per_change() {
    let runs = tempfile::tempdir().unwrap();
    let run_id = recorded(runs.path());
    let dir = runs.path().join(&run_id);
    let file = dir.join(leviath_core::files::RUN_FILE);
    let mut cache: StatCache<RunMeta> = StatCache::default();
    let mut reads = 0;
    for _ in 0..2 {
        let got = cache.get_reading(
            &file,
            || {
                reads += 1;
                read_meta_from(&dir).ok()
            },
            |_| std::time::Duration::ZERO,
        );
        assert_eq!(got.unwrap().run_id, run_id);
    }
    assert_eq!(reads, 1, "an unchanged file is not read again");
    // Inside the recheck window the cache answers without a stat.
    let held = cache.get_reading(&file, || None, |_| std::time::Duration::from_secs(60));
    assert!(held.is_some());
    std::fs::remove_file(&file).unwrap();
    assert!(
        cache
            .get_reading(&file, || None, |_| std::time::Duration::ZERO)
            .is_none()
    );
}

/// A forced cancel of a run with a run file is its last step; a second one
/// finds it finished. A file at the run file's name that is no run file is
/// a write that failed.
#[tokio::test]
async fn a_forced_cancel_is_the_run_files_last_step() {
    let runs = tempfile::tempdir().unwrap();
    let run_id = recorded(runs.path());
    let dir = runs.path().join(&run_id);
    assert_eq!(force_cancel_in(&dir, 5), ForceCancelOutcome::Terminated);
    let meta = read_meta_from(&dir).unwrap();
    assert_eq!(meta.status, RunStatus::Cancelled);
    assert_eq!(meta.updated_at, 5);
    assert_eq!(
        force_cancel_in(&dir, 6),
        ForceCancelOutcome::AlreadyTerminal
    );

    let torn = runs.path().join("torn");
    std::fs::create_dir_all(&torn).unwrap();
    let mut bytes = leviath_runtime::runfile::codec::MAGIC.to_vec();
    bytes.extend_from_slice(b" and nothing else");
    std::fs::write(torn.join(leviath_core::files::RUN_FILE), bytes).unwrap();
    crate::test_support::with_tracing(|| {
        assert_eq!(force_cancel_in(&torn, 5), ForceCancelOutcome::WriteFailed);
    });
}

/// A run directory in the older layout, whose journal shares the run file's
/// name, is cancelled in its `meta.json`.
#[test]
fn a_forced_cancel_of_an_older_run_rewrites_its_metadata() {
    let runs = tempfile::tempdir().unwrap();
    let dir = runs.path().join("old-run");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(leviath_core::files::ARCHIVE_FILE),
        b"LVR1 and the rest",
    )
    .unwrap();
    write_meta_to(&dir, &crate::test_fixtures::fixtures::run_meta("old-run")).unwrap();
    assert_eq!(force_cancel_in(&dir, 9), ForceCancelOutcome::Terminated);
    assert_eq!(read_meta_from(&dir).unwrap().status, RunStatus::Cancelled);
}

/// A run file whose state does not decode lists nothing.
#[tokio::test]
async fn a_run_file_whose_state_does_not_decode_is_not_listed() {
    use leviath_runtime::runfile::codec::{FrameKind, encode};
    let runs = tempfile::tempdir().unwrap();
    let run_id = recorded(runs.path());
    let dir = runs.path().join(&run_id);
    let mut bytes = std::fs::read(dir.join(leviath_core::files::RUN_FILE)).unwrap();
    bytes.extend(encode(FrameKind::Delta, &1u64).unwrap());
    std::fs::write(dir.join(leviath_core::files::RUN_FILE), bytes).unwrap();
    assert!(read_meta_from(&dir).is_err());
}
