use super::*;

/// A directory holding `file` (with `text`) and `sub/inner`.
fn tree(dir: &Path, text: &str) {
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("file"), text).unwrap();
    std::fs::write(dir.join("sub/inner"), "inner").unwrap();
}

#[test]
fn an_installed_blueprint_is_copied_once_and_the_backup_is_announced_once() {
    let home = tempfile::tempdir().unwrap();
    let agents = home.path().join("agents");
    let coder = agents.join("coder");
    tree(&coder, "before");
    let backup = Backup::of_home(home.path());
    assert!(!backup.dir().exists(), "nothing is written before a save");
    let saved = backup.save_blueprint(&coder, Some(&agents)).unwrap();
    assert_eq!(saved, backup.dir().join("agents/coder"));
    assert_eq!(
        std::fs::read_to_string(saved.join("file")).unwrap(),
        "before"
    );
    assert_eq!(
        std::fs::read_to_string(saved.join("sub/inner")).unwrap(),
        "inner"
    );
    // The backup says how to go back with it.
    let readme = std::fs::read_to_string(backup.dir().join(README)).unwrap();
    assert!(
        readme.contains("To go back to the release you upgraded from"),
        "{readme}"
    );
    assert!(readme.contains("runs.unconverted"), "{readme}");
    // A release without a daemon cannot start one: the steps say so.
    assert!(readme.contains("0.1.0 has no daemon"), "{readme}");
    // A second save of the same blueprint keeps the first copy.
    std::fs::write(coder.join("file"), "after").unwrap();
    let again = Backup::of_home(home.path());
    assert_eq!(again.dir(), backup.dir());
    again.save_blueprint(&coder, Some(&agents)).unwrap();
    assert_eq!(
        std::fs::read_to_string(saved.join("file")).unwrap(),
        "before"
    );

    assert!(announce(home.path()).is_empty(), "nothing to tell yet");
    backup.announce_later(&["first".to_string()]);
    backup.announce_later(&["second".to_string()]);
    assert_eq!(announce(home.path()), ["first", "second"]);
    assert!(announce(home.path()).is_empty(), "told once");
    assert!(backup.dir().join("agents/coder").is_dir(), "never deleted");
}

/// A blueprint left as it was is noted as named only where a backup was
/// begun; with none, nothing is written and nothing counts as named.
#[test]
fn blueprints_left_as_they_were_are_noted_in_a_begun_backup_only() {
    let home = tempfile::tempdir().unwrap();
    let backup = Backup::of_home(home.path());
    let broken = ["/h/agents/broken".to_string()];
    assert!(!backup.name_left(&broken));
    assert!(!backup.name_left(&broken), "no backup, so nothing noted");
    assert!(!backup.dir().exists());
    let coder = home.path().join("coder");
    tree(&coder, "x");
    backup.save_blueprint(&coder, None).unwrap();
    assert!(backup.name_left(&[]), "nothing to name");
    assert!(!backup.name_left(&broken));
    assert!(backup.name_left(&broken));
    let both = [broken[0].clone(), "/h/agents/other".to_string()];
    assert!(!backup.name_left(&both));
    assert!(backup.name_left(&both));
}

#[test]
fn a_blueprint_from_an_agent_path_is_kept_apart_by_where_it_was() {
    let home = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let mine = elsewhere.path().join("mine");
    tree(&mine, "x");
    let backup = Backup::of_runs(&home.path().join("runs"));
    let saved = backup.save_blueprint(&mine, None).unwrap();
    assert!(saved.starts_with(backup.dir().join("agent_paths")));
    let name = saved.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("mine-") && name.len() == "mine-".len() + 12,
        "{name}"
    );
    assert!(saved.join("file").is_file());
}

#[test]
fn a_run_is_linked_and_copied_where_a_link_cannot_be_made() {
    let home = tempfile::tempdir().unwrap();
    let runs = home.path().join("runs");
    let run = runs.join("r-1");
    tree(&run, "journal");
    let backup = Backup::of_runs(&runs);
    let saved = backup.save_run(&run).unwrap();
    assert_eq!(saved, backup.dir().join("runs/r-1"));
    assert_eq!(
        std::fs::read_to_string(saved.join("file")).unwrap(),
        "journal"
    );
    // Moving the run's files aside, as a conversion does, leaves the backup.
    std::fs::rename(run.join("file"), runs.join("moved")).unwrap();
    assert_eq!(
        std::fs::read_to_string(saved.join("file")).unwrap(),
        "journal"
    );

    let other = runs.join("r-2");
    tree(&other, "copied");
    let no_links = |_: &Path, _: &Path| Err(std::io::Error::other("no links here"));
    let saved = backup.save_run_with(&other, &no_links).unwrap();
    assert_eq!(
        std::fs::read_to_string(saved.join("file")).unwrap(),
        "copied"
    );
}

/// A run's stage logs stay in its directory after the conversion and a
/// resumed run appends to them, so the backup holds copies of them, never
/// links that would change with the run.
#[test]
fn a_runs_stage_logs_are_copied_so_appending_leaves_the_backup() {
    use std::io::Write;
    let home = tempfile::tempdir().unwrap();
    let runs = home.path().join("runs");
    let run = runs.join("r-1");
    std::fs::create_dir_all(run.join("stages/0")).unwrap();
    std::fs::write(run.join("stages/0/logs.log"), "old\n").unwrap();
    std::fs::write(run.join("meta.json"), "{}").unwrap();
    let saved = Backup::of_runs(&runs).save_run(&run).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(run.join("stages/0/logs.log"))
        .unwrap()
        .write_all(b"new\n")
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(saved.join("stages/0/logs.log")).unwrap(),
        "old\n"
    );
    assert_eq!(
        std::fs::read_to_string(saved.join("meta.json")).unwrap(),
        "{}"
    );
}

#[test]
fn a_save_that_fails_leaves_nothing_behind() {
    let home = tempfile::tempdir().unwrap();
    let backup = Backup::of_home(home.path());
    assert!(backup.save_run(&home.path().join("runs/missing")).is_err());
    let runs = backup.dir().join("runs");
    let left: Vec<_> = std::fs::read_dir(&runs)
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    assert!(left.is_empty(), "{left:?}");
    // A file that cannot be saved stops the save, and the copy made so far
    // goes with it.
    let run = home.path().join("r-0");
    tree(&run, "x");
    let fails = |_: &Path, _: &Path| Err(std::io::Error::other("no room"));
    assert!(backup.save(&run, Path::new("runs/r-0"), &fails).is_err());
    assert!(!backup.dir().join("runs/r-0").exists());

    // A data root where the backups cannot be made.
    let blocked = tempfile::tempdir().unwrap();
    std::fs::write(blocked.path().join(BACKUPS_DIR), "a file").unwrap();
    let run = home.path().join("r");
    tree(&run, "x");
    assert!(Backup::of_home(blocked.path()).save_run(&run).is_err());
    assert!(announce(blocked.path()).is_empty());
}

/// Nothing is kept to tell when nothing was saved, and a summary that
/// cannot be kept is said in the log.
#[test]
fn a_summary_is_kept_only_beside_a_backup_and_a_failure_is_logged() {
    let home = tempfile::tempdir().unwrap();
    let backup = Backup::of_home(home.path());
    backup.announce_later(&["nothing saved".to_string()]);
    assert!(!backup.dir().exists());
    let run = home.path().join("r");
    tree(&run, "x");
    backup.save_run(&run).unwrap();
    std::fs::create_dir_all(backup.dir().join(UNANNOUNCED)).unwrap();
    crate::test_support::with_tracing(|| backup.announce_later(&["lost".to_string()]));
    assert!(backup.dir().join(UNANNOUNCED).is_dir());
}

#[test]
fn a_later_start_of_the_same_release_adds_to_the_backup_it_began() {
    let home = tempfile::tempdir().unwrap();
    let backups = home.path().join(BACKUPS_DIR);
    let earlier = backups.join(format!("{}-100", env!("CARGO_PKG_VERSION")));
    let later = backups.join(format!("{}-200", env!("CARGO_PKG_VERSION")));
    std::fs::create_dir_all(&later).unwrap();
    std::fs::create_dir_all(&earlier).unwrap();
    std::fs::create_dir_all(backups.join("0.0.1-50")).unwrap();
    assert_eq!(Backup::of_home(home.path()).dir(), earlier);
}

#[test]
fn the_backup_of_the_users_own_home_is_told_once() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join(".leviath");
    let run = home.path().join("r");
    tree(&run, "x");
    let backup = Backup::of_home(&root);
    backup.save_run(&run).unwrap();
    backup.announce_later(&["upgraded".to_string()]);
    temp_env::with_vars(
        [("LEVIATH_HOME", Some(home.path().to_str().unwrap()))],
        || {
            assert_eq!(
                leviath_core::paths::data_dir().as_deref(),
                Some(root.as_path())
            );
            tell_once();
            assert!(!backup.dir().join(UNANNOUNCED).exists());
            assert!(backup.dir().join(README).is_file());
        },
    );
}
