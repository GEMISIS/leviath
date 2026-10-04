use super::*;

const TINY: &str = r#"[blueprint]
name = "tiny"
version = "1.0.0"

[graph]
stages = [{ name = "main", system_prompt = "work" }]
layout = { total_budget_tokens = 1000, regions = [{ name = "task", kind = "pinned", budget = 1000 }] }
"#;

fn write(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let file = dir.join(FILE_NAME);
    std::fs::write(&file, TINY).unwrap();
    file
}

#[test]
fn the_file_itself_names_itself() {
    let tmp = tempfile::tempdir().unwrap();
    let file = write(tmp.path());
    let found = find_blueprint_in(file.to_str().unwrap(), None, Path::new("")).unwrap();
    assert_eq!(found, file);
}

#[test]
fn a_directory_names_the_file_inside_it() {
    let tmp = tempfile::tempdir().unwrap();
    let file = write(tmp.path());
    let found = find_blueprint_in(tmp.path().to_str().unwrap(), None, Path::new("")).unwrap();
    assert_eq!(found, file);
}

#[test]
fn a_name_finds_the_installed_blueprint() {
    let agents = tempfile::tempdir().unwrap();
    let file = write(&agents.path().join("named"));
    let found = find_blueprint_in("named", Some(agents.path()), Path::new("")).unwrap();
    assert_eq!(found, file);
}

#[test]
fn the_current_directory_is_the_last_resort() {
    let cwd = tempfile::tempdir().unwrap();
    let agents = tempfile::tempdir().unwrap();
    let file = write(cwd.path());
    let found = find_blueprint_in("absent", Some(agents.path()), cwd.path()).unwrap();
    assert_eq!(found, file);
}

#[test]
fn nothing_found_says_what_to_pass() {
    let empty = tempfile::tempdir().unwrap();
    // A directory with no blueprint inside falls through to the error too.
    let err = find_blueprint_in(
        empty.path().to_str().unwrap(),
        Some(empty.path()),
        empty.path(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains(FILE_NAME), "{err}");
    assert!(err.contains("lev list"), "{err}");

    // One holding only the older manifest says how to convert it.
    std::fs::write(empty.path().join("agent.leviath"), "old").unwrap();
    let err = find_blueprint_in(
        empty.path().to_str().unwrap(),
        Some(empty.path()),
        empty.path(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("lev blueprint migrate"), "{err}");

    // An installed name still in the older format says the same, and the
    // lookup leaves the install exactly as it was.
    let agents = tempfile::tempdir().unwrap();
    let installed = agents.path().join("helper");
    std::fs::create_dir_all(&installed).unwrap();
    std::fs::write(installed.join("agent.leviath"), "old").unwrap();
    let err = find_blueprint_in("helper", Some(agents.path()), Path::new(""))
        .unwrap_err()
        .to_string();
    assert!(err.contains("lev daemon restart"), "{err}");
    // Named once, in the command that converts it, and once more as where
    // that command writes.
    let at = installed.display().to_string();
    assert_eq!(err.matches(&at).count(), 2, "{err}");
    assert!(installed_old_format_in("helper", Some(agents.path())));
    assert!(!installed_old_format_in("helper", None));
    assert!(!installed_old_format("no-such-blueprint-anywhere"));
    assert!(!installed_old_format_in("other", Some(agents.path())));
    assert_eq!(
        std::fs::read(installed.join("agent.leviath")).unwrap(),
        b"old"
    );
    assert!(!installed.join(FILE_NAME).exists());
}

/// The old manifest named as a file says how to convert it too, and only
/// an installed one mentions the daemon's upgrade.
#[test]
fn the_old_manifest_itself_says_how_to_convert_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("helper");
    std::fs::create_dir_all(&dir).unwrap();
    let old = dir.join(OLD_FILE_NAME);
    std::fs::write(&old, "old").unwrap();
    let err = find_blueprint_in(old.to_str().unwrap(), None, Path::new(""))
        .unwrap_err()
        .to_string();
    assert!(err.contains("lev blueprint migrate"), "{err}");
    assert!(!err.contains("lev daemon restart"), "{err}");
    // An `agent.toml` beside it is what every command reads, so the old file
    // is nothing to report.
    std::fs::write(dir.join(FILE_NAME), TINY).unwrap();
    assert_eq!(old_format(&dir, "helper", None), None);
    assert_eq!(old_format(&old, "helper", None), None);
}

#[test]
fn a_file_with_another_name_is_not_a_blueprint() {
    let tmp = tempfile::tempdir().unwrap();
    let other = tmp.path().join("notes.toml");
    std::fs::write(&other, TINY).unwrap();
    assert!(find_blueprint_in(other.to_str().unwrap(), None, tmp.path()).is_err());
}

/// Name lookup goes through the `LEVIATH_HOME`-aware agents dir, so `lev run
/// <name>` finds the same tree `lev add` installs into when the override is
/// set.
#[test]
fn find_blueprint_honors_leviath_home() {
    let home = tempfile::tempdir().unwrap();
    let file = write(&home.path().join(".leviath").join("agents").join("named"));
    temp_env::with_var("LEVIATH_HOME", Some(home.path()), || {
        assert_eq!(find_blueprint("named").unwrap(), file);
    });
}

#[test]
fn loading_reads_the_graph_or_says_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let file = write(tmp.path());
    let loaded = loaded_at(&file).unwrap();
    assert_eq!(loaded.reference.name.as_str(), "tiny");
    assert_eq!(loaded_at(tmp.path()).unwrap(), loaded);
    assert!(loaded_at(&tmp.path().join("nope")).is_none());
}
