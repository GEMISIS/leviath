use super::*;

use crate::runstate::create_run_in;

fn meta(id: &str, title: &str, started_at: i64) -> RunMeta {
    let mut meta = RunMeta::new(
        id.to_string(),
        "agent".to_string(),
        "/agents/agent".to_string(),
        "task".to_string(),
        None,
        "/work".to_string(),
        1,
    );
    meta.title = Some(title.to_string());
    meta.started_at = started_at;
    meta
}

/// A runs directory with two runs and a directory that holds no run.
fn runs() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let runs = home.path().join("runs");
    create_run_in(&runs.join("old-run"), &meta("old-run", "older", 100)).unwrap();
    create_run_in(&runs.join("new-run"), &meta("new-run", "newer", 200)).unwrap();
    std::fs::create_dir_all(runs.join("junk")).unwrap();
    home
}

fn titles(runs: &[RunMeta]) -> Vec<String> {
    runs.iter()
        .map(|r| r.title.clone().unwrap_or_default())
        .collect()
}

/// Edit the saved index as JSON.
fn edit_index(runs_dir: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let path = path_for(runs_dir);
    let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    edit(&mut v);
    std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
}

/// The first listing builds the index beside the runs; the next one answers
/// from it while the run files are as they were, and reads a run again once
/// its file changes.
#[test]
fn a_listing_reads_a_run_file_only_when_it_changed() {
    let home = runs();
    let dir = home.path().join("runs");
    assert_eq!(titles(&list(&dir)), ["newer", "older"]);
    assert_eq!(path_for(&dir), home.path().join("runs.index"));
    assert!(path_for(&dir).is_file());

    edit_index(&dir, |v| {
        v["runs"]["old-run"]["run"]["title"] = "from the index".into();
    });
    assert_eq!(titles(&list(&dir)), ["newer", "from the index"]);

    std::thread::sleep(Duration::from_millis(5));
    create_run_in(&dir.join("old-run"), &meta("old-run", "renamed", 100)).unwrap();
    assert_eq!(titles(&list(&dir)), ["newer", "renamed"]);
}

/// A run that is gone, or whose file no longer reads, leaves the index; an
/// index that does not parse, or is in another format, is built again.
#[test]
fn the_index_follows_runs_that_go_and_rebuilds_when_unreadable() {
    let home = runs();
    let dir = home.path().join("runs");
    list(&dir);
    std::fs::remove_dir_all(dir.join("new-run")).unwrap();
    std::fs::write(
        dir.join("old-run").join(leviath_core::files::RUN_FILE),
        b"not a run file",
    )
    .unwrap();
    assert!(list(&dir).is_empty());
    let saved: IndexFile = serde_json::from_slice(&std::fs::read(path_for(&dir)).unwrap()).unwrap();
    assert!(saved.runs.is_empty());

    create_run_in(&dir.join("old-run"), &meta("old-run", "back", 100)).unwrap();
    std::fs::write(path_for(&dir), b"{ not json").unwrap();
    assert_eq!(titles(&list(&dir)), ["back"]);
    edit_index(&dir, |v| {
        v["version"] = 0.into();
        v["runs"]["old-run"]["run"]["title"] = "stale".into();
    });
    assert_eq!(titles(&list(&dir)), ["back"]);
}

/// No runs directory lists nothing and writes nothing; an index that cannot
/// be written costs nothing but the next listing's reads.
#[test]
fn no_runs_and_an_unwritable_index_are_harmless() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("runs");
    assert!(list(&dir).is_empty());
    assert!(!path_for(&dir).exists());

    let home = runs();
    let dir = home.path().join("runs");
    std::fs::create_dir_all(path_for(&dir)).unwrap();
    assert_eq!(titles(&list(&dir)), ["newer", "older"]);
    assert!(path_for(&dir).is_dir());
    assert!(path_for(Path::new("/")).ends_with("runs.index"));
    let mut index = RunIndex::load(&dir);
    assert!(index.run(Path::new("/")).is_none());
}

/// The daemon keeps the index up to date on its own.
#[tokio::test]
async fn the_daemon_keeps_the_index_fresh() {
    let home = runs();
    let dir = home.path().join("runs");
    keep_fresh(&tokio::runtime::Handle::current(), dir.clone());
    for _ in 0..200 {
        if path_for(&dir).is_file() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let saved: IndexFile = serde_json::from_slice(&std::fs::read(path_for(&dir)).unwrap()).unwrap();
    assert_eq!(saved.runs.len(), 2);
}
