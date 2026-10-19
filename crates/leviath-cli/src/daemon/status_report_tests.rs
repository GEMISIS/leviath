use super::*;
use leviath_runtime::control_socket::DaemonIdentity;

/// The `lev` asking, as a git build of 0.6.4.
fn cli() -> Build {
    Build {
        id: "93edfd0c".to_string(),
        version: Some("0.6.4".to_string()),
        committed_at: Some(1_792_050_483),
    }
}

/// A running daemon that introduced itself, whose marker reads `marker`.
fn running(marker: &str) -> Observed {
    Observed {
        running: true,
        marker: Some(marker.to_string()),
        identity: Some(DaemonIdentity {
            pid: 41230,
            exe: Some("/Users/me/.cargo/bin/lev".to_string()),
            cwd: Some("/Users/me/.leviath".to_string()),
            ..DaemonIdentity::this_process("87296d6e")
        }),
        runs: 3,
        supervised: Some((
            false,
            std::path::PathBuf::from("/agents/dev.leviath.daemon.plist"),
        )),
        cli_exe: Some("/usr/local/bin/lev".to_string()),
        mode: DaemonMode::default(),
    }
}

/// `spawn_would_replace` answers for the mode this host's next spawn runs
/// in: `LEVIATH_DAEMON=attach` replaces nothing, and `replace` also replaces
/// a build it cannot put in order.
#[test]
fn spawn_would_replace_follows_the_hosts_mode() {
    let older = |mode| {
        report(
            Observed {
                mode,
                ..running("87296d6e 0.6.4 1791951523")
            },
            &cli(),
        )
        .spawn_would_replace
    };
    assert!(older(DaemonMode::Ensure));
    assert!(!older(DaemonMode::Attach));
    let unordered = report(
        Observed {
            mode: DaemonMode::Replace,
            ..running("v0.6.4 0.6.4")
        },
        &cli(),
    );
    assert!(unordered.spawn_would_replace);
}

/// The object the issue asks for, field for field: the daemon's pid and
/// executable from its handshake, its build from its marker, this `lev`'s
/// build, how they stand, and that the default mode would replace an older
/// daemon.
#[test]
fn a_running_older_daemon_reports_every_field() {
    let report = report(running("87296d6e 0.6.4 1791951523"), &cli());
    let json: serde_json::Value = serde_json::from_str(&to_json(&report)).unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "running": true,
            "pid": 41230,
            "exe": "/Users/me/.cargo/bin/lev",
            "cwd": "/Users/me/.leviath",
            "version": "0.6.4",
            "build": "87296d6e",
            "committed_at": 1_791_951_523_u64,
            "runs": 3,
            "supervised": {
                "installed": false,
                "path": "/agents/dev.leviath.daemon.plist"
            },
            "cli": {
                "exe": "/usr/local/bin/lev",
                "version": "0.6.4",
                "build": "93edfd0c",
                "committed_at": 1_792_050_483_u64
            },
            "standing": "older",
            "spawn_would_replace": true
        })
    );
}

/// A build of the same version made without git cannot be put in order, so
/// the default mode leaves it running, and says so.
#[test]
fn an_unordered_daemon_is_not_replaced_by_default() {
    let report = report(running("v0.6.4 0.6.4"), &cli());
    assert_eq!(report.standing, Some(Standing::Unordered));
    assert_eq!(report.committed_at, None);
    assert!(!report.spawn_would_replace);
    assert!(to_json(&report).contains(r#""standing": "unordered""#));
}

/// With no daemon running, everything about the daemon is null, whatever a
/// stale marker or handshake says, and a spawn would start one, not replace
/// one. The `lev` asking is reported all the same.
#[test]
fn no_daemon_running_reports_nulls_and_no_replacement() {
    let observed = Observed {
        running: false,
        runs: 0,
        supervised: None,
        ..running("87296d6e 0.6.4 1791951523")
    };
    let report = report(observed, &cli());
    let json: serde_json::Value = serde_json::from_str(&to_json(&report)).unwrap();
    for field in [
        "pid",
        "exe",
        "cwd",
        "version",
        "build",
        "committed_at",
        "supervised",
        "standing",
    ] {
        assert_eq!(json[field], serde_json::Value::Null, "{field}");
    }
    assert_eq!(json["running"], false);
    assert_eq!(json["spawn_would_replace"], false);
    assert_eq!(json["cli"]["build"], "93edfd0c");
}

/// A daemon too old to write a marker or say who it is reports its fields as
/// null, and stands older, so the default mode replaces it.
#[test]
fn a_daemon_that_said_nothing_is_older() {
    let observed = Observed {
        marker: None,
        identity: None,
        ..running("")
    };
    let report = report(observed, &cli());
    assert_eq!((report.pid, report.exe, report.cwd), (None, None, None));
    assert_eq!((report.version, report.build), (None, None));
    assert_eq!(report.standing, Some(Standing::Older));
    assert!(report.spawn_would_replace);
}

/// Nothing secret reaches the output: the handshake's token never crosses
/// into the identity, and the tool credentials it lists are not printed.
#[test]
fn the_report_carries_no_secret() {
    let mut observed = running("87296d6e 0.6.4 1791951523");
    observed.identity = observed
        .identity
        .map(|identity| identity.with_tool_env(vec!["BRAVE_API_KEY".to_string()]));
    let json = to_json(&report(observed, &cli()));
    assert!(!json.contains("BRAVE"), "{json}");
    assert!(!json.contains("token"), "{json}");
}

/// A daemon that introduced itself without a working directory, as one built
/// before the field was added does, reports `cwd` as null and the rest as
/// usual.
#[test]
fn a_daemon_that_did_not_report_its_directory_has_a_null_cwd() {
    let mut observed = running("87296d6e 0.6.4 1791951523");
    observed.identity = observed.identity.map(|identity| DaemonIdentity {
        cwd: None,
        ..identity
    });
    let report = report(observed, &cli());
    assert_eq!(report.cwd, None);
    assert_eq!(report.pid, Some(41230));
    let json: serde_json::Value = serde_json::from_str(&to_json(&report)).unwrap();
    assert_eq!(json["cwd"], serde_json::Value::Null);
}
