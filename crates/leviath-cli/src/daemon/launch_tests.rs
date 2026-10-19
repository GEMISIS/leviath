use super::*;

/// A starter environment holding exactly `vars`.
fn vars(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
    let vars: Vec<(String, String)> = vars
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    move |name| {
        vars.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| OsString::from(v))
    }
}

/// A working directory that is absolute on every platform.
fn starter_cwd() -> PathBuf {
    std::env::temp_dir().join("starter")
}

/// The spawned daemon runs in the directory it is given, whatever this
/// process's own working directory is.
#[test]
fn the_daemon_starts_in_the_given_directory() {
    let dir = tempfile::tempdir().unwrap();
    let cmd = daemon_command(Path::new("/opt/lev"), dir.path(), &ChildEnv::default());
    assert_eq!(cmd.get_current_dir(), Some(dir.path()));
    assert_ne!(
        cmd.get_current_dir(),
        Some(std::env::current_dir().unwrap().as_path())
    );
    assert_eq!(cmd.get_program(), "/opt/lev");
    assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["daemon"]);
}

/// The serve API token and what the starter loaded from a `.env` are removed
/// from the daemon's environment, and nothing else is touched: `PATH` and
/// exported provider keys still come from the starter.
#[test]
fn the_serve_token_and_dotenv_names_are_removed_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let env = ChildEnv::of(
        vars(&[("PATH", "/usr/bin"), ("LEVIATH_HOME", "/home/me")]),
        &starter_cwd(),
        vec!["BRAVE_API_KEY".to_string()],
    );
    let cmd = daemon_command(Path::new("/opt/lev"), dir.path(), &env);
    let envs: Vec<_> = cmd.get_envs().collect();
    assert_eq!(
        envs,
        [
            (std::ffi::OsStr::new("BRAVE_API_KEY"), None),
            (std::ffi::OsStr::new("LEVIATH_API_TOKEN"), None),
        ],
        "only the serve token and the .env names change, and they are removed"
    );
}

/// A relative `LEVIATH_HOME` (and the other path variables) is handed to the
/// daemon resolved against the starter's directory, since the daemon no
/// longer runs there. An absolute one is left alone.
#[test]
fn relative_path_variables_are_made_absolute_for_the_child() {
    let cwd = starter_cwd();
    let env = ChildEnv::of(
        vars(&[
            ("LEVIATH_HOME", "h"),
            ("LEVIATH_CONFIG_PATH", "conf/config.toml"),
            ("LEVIATH_RUNS_DIR", "runs"),
            ("LEVIATH_DUMP_REQUEST_DIR", "dumps"),
        ]),
        &cwd,
        Vec::new(),
    );
    assert_eq!(
        env.set,
        [
            ("LEVIATH_HOME", cwd.join("h")),
            ("LEVIATH_CONFIG_PATH", cwd.join("conf/config.toml")),
            ("LEVIATH_RUNS_DIR", cwd.join("runs")),
            ("LEVIATH_DUMP_REQUEST_DIR", cwd.join("dumps")),
        ]
    );
    let dir = tempfile::tempdir().unwrap();
    let cmd = daemon_command(Path::new("/opt/lev"), dir.path(), &env);
    let home = cmd
        .get_envs()
        .find(|(name, _)| *name == "LEVIATH_HOME")
        .and_then(|(_, value)| value)
        .map(PathBuf::from);
    assert_eq!(home, Some(cwd.join("h")));

    let absolute = cwd.join("home").display().to_string();
    let env = ChildEnv::of(vars(&[("LEVIATH_HOME", &absolute)]), &cwd, Vec::new());
    assert!(env.set.is_empty(), "{:?}", env.set);
}

/// The token and a `.env` name really are gone from a child started this way,
/// even when the starter had them set, while an ordinary variable still
/// arrives and a relative home arrives absolute.
#[cfg(unix)]
#[test]
fn a_child_started_this_way_sees_the_fixed_environment() {
    let dir = tempfile::tempdir().unwrap();
    let env = ChildEnv::of(
        vars(&[("LEVIATH_HOME", "h")]),
        Path::new("/starter"),
        vec!["LEV_FROM_DOTENV".to_string()],
    );
    let cmd = daemon_command(Path::new("/usr/bin/env"), dir.path(), &env);
    let mut env_cmd = Command::new("/usr/bin/env");
    env_cmd
        .env("LEVIATH_API_TOKEN", "starter-token")
        .env("LEV_FROM_DOTENV", "from-project")
        .env("LEVIATH_LAUNCH_PROBE", "kept");
    for (name, value) in cmd.get_envs() {
        match value {
            Some(value) => env_cmd.env(name, value),
            None => env_cmd.env_remove(name),
        };
    }
    let out = env_cmd.output().unwrap();
    let seen = String::from_utf8(out.stdout).unwrap();
    assert!(!seen.contains("LEVIATH_API_TOKEN"), "{seen}");
    assert!(!seen.contains("LEV_FROM_DOTENV"), "{seen}");
    assert!(seen.contains("LEVIATH_LAUNCH_PROBE=kept"), "{seen}");
    assert!(seen.contains("LEVIATH_HOME=/starter/h"), "{seen}");
}

/// The Windows twin: `cmd /C set` lists the child's environment.
#[cfg(windows)]
#[test]
fn a_child_started_this_way_sees_the_fixed_environment() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = starter_cwd();
    let env = ChildEnv::of(
        vars(&[("LEVIATH_HOME", "h")]),
        &cwd,
        vec!["LEV_FROM_DOTENV".to_string()],
    );
    let cmd = daemon_command(Path::new("cmd"), dir.path(), &env);
    let mut env_cmd = Command::new("cmd");
    env_cmd
        .args(["/C", "set"])
        .env("LEVIATH_API_TOKEN", "starter-token")
        .env("LEV_FROM_DOTENV", "from-project")
        .env("LEVIATH_LAUNCH_PROBE", "kept");
    for (name, value) in cmd.get_envs() {
        match value {
            Some(value) => env_cmd.env(name, value),
            None => env_cmd.env_remove(name),
        };
    }
    let out = env_cmd.output().unwrap();
    let seen = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(!seen.contains("LEVIATH_API_TOKEN"), "{seen}");
    assert!(!seen.contains("LEV_FROM_DOTENV"), "{seen}");
    assert!(seen.contains("LEVIATH_LAUNCH_PROBE=kept"), "{seen}");
    assert!(
        seen.contains(&format!("LEVIATH_HOME={}", cwd.join("h").display())),
        "{seen}"
    );
}

/// The foreground daemon refuses a relative path variable, naming it, since
/// it cannot hand itself an absolute one; absolute or unset ones pass.
#[test]
fn the_foreground_daemon_refuses_a_relative_path_variable() {
    let err = refuse_relative_paths(vars(&[("LEVIATH_HOME", "h")]))
        .unwrap_err()
        .to_string();
    assert!(err.contains("LEVIATH_HOME is a relative path (h)"), "{err}");
    let err = refuse_relative_paths(vars(&[("LEVIATH_RUNS_DIR", "runs")]))
        .unwrap_err()
        .to_string();
    assert!(err.contains("LEVIATH_RUNS_DIR"), "{err}");
    let absolute = starter_cwd().display().to_string();
    assert!(refuse_relative_paths(vars(&[("LEVIATH_HOME", &absolute)])).is_ok());
    assert!(refuse_relative_paths(vars(&[])).is_ok());
}

/// A relative data directory comes back absolute, resolved against the
/// starter's directory: the child resolving it against its own new working
/// directory nested `h/.leviath/h/.leviath` and bound the wrong socket.
#[test]
fn a_relative_data_dir_comes_back_absolute() {
    let cwd = tempfile::tempdir().unwrap();
    let dir = daemon_dir(Some(PathBuf::from("h/.leviath")), cwd.path()).unwrap();
    assert_eq!(dir, cwd.path().join("h/.leviath"));
    assert!(dir.is_dir());
}

/// The data directory is created when it is missing, so the very first
/// daemon on a machine has somewhere to start.
#[test]
fn the_daemon_dir_is_created_when_missing() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join(".leviath");
    assert_eq!(
        daemon_dir(Some(data.clone()), &starter_cwd()).unwrap(),
        data
    );
    assert!(data.is_dir());
    // And an existing one is used as it is.
    assert_eq!(
        daemon_dir(Some(data.clone()), &starter_cwd()).unwrap(),
        data
    );
}

/// With no home to put the data directory in, starting a daemon is refused
/// with a reason rather than falling back to wherever `lev` was run.
#[test]
fn no_home_is_refused() {
    let err = daemon_dir(None, &starter_cwd()).unwrap_err().to_string();
    assert!(err.contains("home directory"), "{err}");
}

/// A data directory that cannot be created is an error, not a daemon
/// started somewhere else.
#[test]
fn an_uncreatable_dir_is_an_error() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join("not-a-dir");
    std::fs::write(&file, "").unwrap();
    assert!(daemon_dir(Some(file.join(".leviath")), &starter_cwd()).is_err());
}
