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

/// What a daemon brings back when it starts is found through the index: a
/// run that finished is not among them, and a directory still holding the
/// file an earlier release wrote is passed over from its first bytes.
#[test]
fn a_start_reads_only_the_runs_that_have_not_finished() {
    let home = runs();
    let dir = home.path().join("runs");
    let mut done = meta("done-run", "done", 300);
    done.status = leviath_core::run_meta::RunStatus::Complete;
    create_run_in(&dir.join("done-run"), &done).unwrap();
    std::fs::create_dir_all(dir.join("old-layout")).unwrap();
    std::fs::write(
        dir.join("old-layout").join(leviath_core::files::RUN_FILE),
        b"LVR1 a journal from an earlier release",
    )
    .unwrap();

    let open = unfinished(&dir);
    let names: Vec<String> = open
        .iter()
        .map(|d| d.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["new-run", "old-run"]);
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path_for(&dir)).unwrap()).unwrap();
    assert!(saved["runs"]["done-run"].is_object(), "{saved}");
    assert!(saved["runs"]["old-layout"].is_null(), "{saved}");
    assert!(unfinished(&home.path().join("gone")).is_empty());
}

/// Whether the saved index still has the run id of `name`'s summary: a start
/// that answered from the index at a glance never rewrites it, and one that
/// had to read the index whole writes it back complete.
fn has_run_id(runs_dir: &Path, name: &str) -> bool {
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path_for(runs_dir)).unwrap()).unwrap();
    saved["runs"][name]["run"]["run_id"].is_string()
}

fn names(dirs: &[PathBuf]) -> Vec<String> {
    dirs.iter()
        .map(|d| d.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

/// A start whose index is up to date finds the unfinished runs from each
/// one's status alone, without making a summary of every run; once a run
/// file has changed, or the index names a run that is gone, the index is
/// read whole and brought up to date.
#[test]
fn a_start_reads_the_index_at_a_glance_while_it_is_up_to_date() {
    let home = runs();
    let dir = home.path().join("runs");
    let mut done = meta("done-run", "done", 300);
    done.status = leviath_core::run_meta::RunStatus::Complete;
    create_run_in(&dir.join("done-run"), &done).unwrap();
    list(&dir);
    // A summary without its run id does not read whole, so a start that
    // made summaries would read the index as empty and write it again.
    let strip = |v: &mut serde_json::Value| {
        v["runs"]["old-run"]["run"]
            .as_object_mut()
            .unwrap()
            .remove("run_id");
    };
    edit_index(&dir, strip);
    assert_eq!(names(&unfinished(&dir)), ["new-run", "old-run"]);
    assert!(!has_run_id(&dir, "old-run"), "answered at a glance");

    std::thread::sleep(Duration::from_millis(5));
    create_run_in(&dir.join("new-run"), &meta("new-run", "rewritten", 200)).unwrap();
    assert_eq!(names(&unfinished(&dir)), ["new-run", "old-run"]);
    assert!(
        has_run_id(&dir, "old-run"),
        "a changed run file reads it whole"
    );

    edit_index(&dir, strip);
    std::fs::remove_dir_all(dir.join("done-run")).unwrap();
    assert_eq!(names(&unfinished(&dir)), ["new-run", "old-run"]);
    assert!(
        has_run_id(&dir, "old-run"),
        "a run that is gone reads it whole"
    );

    edit_index(&dir, |v| v["version"] = 0.into());
    assert_eq!(names(&unfinished(&dir)), ["new-run", "old-run"]);
    std::fs::write(path_for(&dir), b"{ not json").unwrap();
    assert_eq!(names(&unfinished(&dir)), ["new-run", "old-run"]);

    // An index beside a runs directory that is gone answers nothing.
    let gone = home.path().join("gone");
    std::fs::write(path_for(&gone), br#"{"version":1,"runs":{}}"#).unwrap();
    assert!(unfinished(&gone).is_empty());
}

/// A refresh reads nothing but the directory while every run file looks as
/// it did: an idle daemon never loads the index. Once a run is added or
/// written, the index is brought up to date.
#[test]
fn a_refresh_reads_the_index_only_when_a_run_file_changed() {
    let home = runs();
    let dir = home.path().join("runs");
    list(&dir);
    let seen = look_of(&dir);
    assert_eq!(
        look_of(&dir),
        seen,
        "nothing changed, so neither did the look"
    );
    // An index this cannot read would be rebuilt by any read of it.
    std::fs::write(path_for(&dir), b"left alone").unwrap();
    assert_eq!(refresh(&dir, seen), seen);
    assert_eq!(std::fs::read(path_for(&dir)).unwrap(), b"left alone");

    create_run_in(&dir.join("third-run"), &meta("third-run", "third", 300)).unwrap();
    let now = refresh(&dir, seen);
    assert_ne!(now, seen);
    let saved: IndexFile = serde_json::from_slice(&std::fs::read(path_for(&dir)).unwrap()).unwrap();
    assert_eq!(saved.runs.len(), 3);
    assert_eq!(look_of(&home.path().join("gone")), 0);
}

/// The daemon keeps the index up to date on its own: a run added while it
/// runs is in the index a moment later.
#[tokio::test]
async fn the_daemon_keeps_the_index_fresh() {
    let home = runs();
    let dir = home.path().join("runs");
    list(&dir);
    keep_fresh_every(
        &tokio::runtime::Handle::current(),
        dir.clone(),
        Duration::from_millis(10),
    );
    create_run_in(&dir.join("third-run"), &meta("third-run", "third", 300)).unwrap();
    let indexed = || -> usize {
        serde_json::from_slice::<IndexFile>(&std::fs::read(path_for(&dir)).unwrap())
            .map_or(0, |saved| saved.runs.len())
    };
    for _ in 0..500 {
        if indexed() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(indexed(), 3);
}
