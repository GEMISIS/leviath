use super::*;

/// The spawned daemon runs in the directory it is given, whatever this
/// process's own working directory is.
#[test]
fn the_daemon_starts_in_the_given_directory() {
    let dir = tempfile::tempdir().unwrap();
    let cmd = daemon_command(Path::new("/opt/lev"), dir.path());
    assert_eq!(cmd.get_current_dir(), Some(dir.path()));
    assert_ne!(
        cmd.get_current_dir(),
        Some(std::env::current_dir().unwrap().as_path())
    );
    assert_eq!(cmd.get_program(), "/opt/lev");
    assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["daemon"]);
}

/// The serve API token is removed from the daemon's environment, and nothing
/// else is touched: `PATH` and provider keys still come from the starter.
#[test]
fn the_serve_token_is_removed_and_nothing_else_is_changed() {
    let dir = tempfile::tempdir().unwrap();
    let cmd = daemon_command(Path::new("/opt/lev"), dir.path());
    let envs: Vec<_> = cmd.get_envs().collect();
    assert_eq!(
        envs,
        [(std::ffi::OsStr::new("LEVIATH_API_TOKEN"), None)],
        "only the serve token is changed, and it is removed"
    );
}

/// The token really is gone from a child started this way, even when the
/// starter had it set, while an ordinary variable still arrives.
#[cfg(unix)]
#[test]
fn a_child_started_this_way_does_not_see_the_token() {
    let mut cmd = Command::new("/usr/bin/env");
    cmd.env("LEVIATH_API_TOKEN", "starter-token")
        .env("LEVIATH_LAUNCH_PROBE", "kept");
    let out = without_serve_env(&mut cmd).output().unwrap();
    let env = String::from_utf8(out.stdout).unwrap();
    assert!(!env.contains("LEVIATH_API_TOKEN"), "{env}");
    assert!(env.contains("LEVIATH_LAUNCH_PROBE=kept"), "{env}");
}

/// The Windows twin: `cmd /C set` lists the child's environment.
#[cfg(windows)]
#[test]
fn a_child_started_this_way_does_not_see_the_token() {
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", "set"])
        .env("LEVIATH_API_TOKEN", "starter-token")
        .env("LEVIATH_LAUNCH_PROBE", "kept");
    let out = without_serve_env(&mut cmd).output().unwrap();
    let env = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(!env.contains("LEVIATH_API_TOKEN"), "{env}");
    assert!(env.contains("LEVIATH_LAUNCH_PROBE=kept"), "{env}");
}

/// The data directory is created when it is missing, so the very first
/// daemon on a machine has somewhere to start.
#[test]
fn the_daemon_dir_is_created_when_missing() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join(".leviath");
    assert_eq!(daemon_dir(Some(data.clone())).unwrap(), data);
    assert!(data.is_dir());
    // And an existing one is used as it is.
    assert_eq!(daemon_dir(Some(data.clone())).unwrap(), data);
}

/// With no home to put the data directory in, starting a daemon is refused
/// with a reason rather than falling back to wherever `lev` was run.
#[test]
fn no_home_is_refused() {
    let err = daemon_dir(None).unwrap_err().to_string();
    assert!(err.contains("home directory"), "{err}");
}

/// A data directory that cannot be created is an error, not a daemon
/// started somewhere else.
#[test]
fn an_uncreatable_dir_is_an_error() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join("not-a-dir");
    std::fs::write(&file, "").unwrap();
    assert!(daemon_dir(Some(file.join(".leviath"))).is_err());
}
