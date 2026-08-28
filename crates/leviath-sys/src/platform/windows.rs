//! Windows implementations.
//!
//! Windows has no POSIX mode bits; access control is an ACL on each object. The
//! hardening this crate applies on Unix - "owner only, nobody else" - has a
//! direct Windows equivalent, and until now it simply was not applied: every
//! `secure_file_perms` call was a silent no-op, so `config.toml` (every provider
//! API key) and `mcp-auth.json` (OAuth access *and refresh* tokens) carried
//! whatever ACL they inherited from their parent.
//!
//! # Why `icacls` rather than the Win32 API
//!
//! Setting an ACL directly means `SetNamedSecurityInfoW` and hand-built
//! security descriptors through raw FFI, and this workspace is
//! `unsafe_code = "forbid"` from top to bottom. `icacls` is the tool Windows
//! ships for exactly this, is present on every supported version, and is
//! reachable with an ordinary `Command` - no `unsafe`, and no new dependency
//! whose own soundness would have to be taken on trust.
//!
//! It is resolved from the system directory rather than `PATH`. A `PATH` lookup
//! for a security-critical helper is itself a way in: drop an `icacls.exe`
//! earlier in `PATH` and the "hardening" step runs attacker code instead.
//!
//! # The ordering that matters
//!
//! A file is created with the ACL it inherits from its directory, so restricting
//! the *directory* first is what closes the window - a secret written into an
//! already-restricted directory is never briefly readable. `write_with_mode`
//! also restricts the file itself afterwards, as defence in depth for a
//! directory that was somehow left permissive.

use std::io;
use std::path::{Path, PathBuf};

/// Restrict `path` so only its owner may read or write it.
///
/// The `mode` is ignored: every caller passes `0o600` or `0o700`, and both mean
/// the same thing here. Translating arbitrary POSIX bits into an ACL would be
/// inventing a mapping no caller asks for.
pub(crate) fn set_mode(path: &Path, _mode: u32) -> io::Result<()> {
    restrict_to_owner(path)
}

/// Write `contents` to `path`, then restrict it to its owner.
///
/// Unlike the Unix path this cannot set the ACL *at creation* without FFI, so
/// there is a window between the write and the restriction. It is closed in
/// practice by the directory: `~/.leviath` is restricted when it is created, and
/// a file inherits its directory's ACL, so the file is owner-only from the
/// moment it exists. This call is what guarantees it even when the directory
/// was not.
pub(crate) fn write_with_mode(path: &Path, contents: &[u8], _mode: u32) -> io::Result<()> {
    std::fs::write(path, contents)?;
    restrict_to_owner(path)
}

/// Open `path` for appending, restricting it to the owner.
///
/// `restrict_to_owner` spawns `icacls`, which is far too heavy to repeat on
/// every appended line, so it runs only when this call is the one that creates
/// the file. An existing file was already restricted when it was created.
///
/// **The restriction is best-effort, unlike [`write_with_mode`]'s.** That one
/// guards secrets - an API key written unprotected is worse than not written at
/// all, so its failure propagates. These two are for a run's own files, which
/// until now were created at the default and never restricted. Failing the
/// create on a failed `icacls` would trade "less protected than intended" for
/// "the run cannot record anything", which is the worse of the two, and it
/// would make persistence depend on spawning a process.
pub(crate) fn open_append_with_mode(path: &Path, _mode: u32) -> io::Result<std::fs::File> {
    let existed = path.exists();
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if !existed {
        let _ = restrict_to_owner(path);
    }
    Ok(file)
}

/// Create `path` and any missing parents, restricting the leaf to the owner.
///
/// Best-effort on the restriction, for the reason [`open_append_with_mode`]
/// gives.
pub(crate) fn create_dir_all_with_mode(path: &Path, _mode: u32) -> io::Result<()> {
    // `is_dir`, not `exists`: a *file* sitting where the directory should be is
    // an error, and `create_dir_all` is what reports it. Returning early on
    // `exists` would call that a success here and fail on Unix, where the same
    // call goes straight to the directory builder.
    if path.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(path)?;
    let _ = restrict_to_owner(path);
    Ok(())
}

/// Tighten `path` if it exists, reporting whether anything changed.
///
/// Windows exposes no cheap "is this permissive?" question comparable to
/// reading a mode, so this restricts unconditionally and reports `None`: the
/// caller uses the answer only to log that it tightened something, and claiming
/// a previous mode that was never read would be a fiction.
pub(crate) fn ensure_private(path: &Path, _mode: u32) -> io::Result<Option<u32>> {
    if !path.exists() {
        return Ok(None);
    }
    restrict_to_owner(path)?;
    Ok(None)
}

/// No process groups on Windows, but detaching still means something here: a
/// child launched with no console of its own is the Windows equivalent of
/// escaping the launching terminal, so it neither inherits Ctrl+C nor pops a
/// window when the parent has no console to share. The caller kills the direct
/// child.
pub(crate) fn configure_detached(cmd: &mut std::process::Command) {
    hide_console_window(cmd);
}

/// Start `cmd` without a console window.
///
/// Windows gives a console application a console. When the parent has one, the
/// child shares it and nothing is drawn; when it does not - the daemon started
/// from Explorer, a service, or a UI console - the OS allocates a fresh window
/// and shows it on the interactive desktop. An agent calling the `shell` tool
/// runs `cmd.exe` dozens of times per run, so that is a flood of flashing
/// windows, and concurrent agents make it worse (issue #228).
///
/// `CREATE_NO_WINDOW` says "this is a console application, run it without a
/// console window". It is right only for a child whose stdio is already piped
/// or nulled, which is every caller here; a child meant to share the user's
/// terminal (the editor) must not get it, or it opens with nowhere to draw.
pub(crate) fn hide_console_window(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    // <https://learn.microsoft.com/windows/win32/procthread/process-creation-flags>
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

/// Windows has no POSIX uid; the value only addresses a per-user
/// launchd/systemd domain, neither of which exists here.
pub(crate) fn current_uid() -> u32 {
    0
}

/// No process groups to signal on this platform.
pub(crate) fn kill_process_group(_pgid: u32) -> io::Result<()> {
    Ok(())
}

// ── The pieces, split so each is testable ────────────────────────────────────

/// Remove every inherited permission from `path` and grant only its owner.
fn restrict_to_owner(path: &Path) -> io::Result<()> {
    let user = resolve_user(std::env::var("USERNAME").ok())?;
    let mut cmd = std::process::Command::new(icacls_program());
    cmd.args(icacls_args(path, &user));
    // Every secret write hardens its file, so this runs often enough to be
    // noticed if each one flashed a console.
    hide_console_window(&mut cmd);
    let output = cmd.output()?;
    interpret_icacls(output.status.success(), &output.stderr, path)
}

/// `icacls.exe` from the system directory, never from `PATH`.
fn icacls_program() -> PathBuf {
    // `SystemRoot` is set on every Windows install; the bare name is a fallback
    // for an environment that has stripped it, not the expected path.
    match std::env::var("SystemRoot") {
        Ok(root) => PathBuf::from(root).join("System32").join("icacls.exe"),
        Err(_) => PathBuf::from("icacls.exe"),
    }
}

/// The arguments that restrict `path` to `user`.
///
/// `/inheritance:r` drops the ACEs inherited from the parent - without it a
/// permissive parent keeps granting access no matter what is added here.
/// `/grant:r` *replaces* any existing grant for the user rather than adding to
/// it, so repeating the call is idempotent.
fn icacls_args(path: &Path, user: &str) -> Vec<String> {
    vec![
        path.display().to_string(),
        "/inheritance:r".to_string(),
        "/grant:r".to_string(),
        format!("{user}:(F)"),
    ]
}

/// The account to grant, or an error explaining why there is none.
///
/// An empty `USERNAME` is treated as absent: granting `":(F)"` would be a
/// malformed rule that `icacls` rejects with a message about syntax, which tells
/// the user nothing about the actual problem.
fn resolve_user(username: Option<String>) -> io::Result<String> {
    match username {
        Some(user) if !user.trim().is_empty() => Ok(user),
        _ => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "cannot restrict a file to its owner: USERNAME is not set, so there is \
             no account to grant. Leviath's secrets would be left readable by other \
             users on this machine.",
        )),
    }
}

/// Turn an `icacls` exit into a `Result` that names the file it was protecting.
fn interpret_icacls(success: bool, stderr: &[u8], path: &Path) -> io::Result<()> {
    if success {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "failed to restrict '{}' to its owner: {}",
        path.display(),
        String::from_utf8_lossy(stderr).trim()
    )))
}

/// Serializes every test that touches the process environment this module
/// reads: `USERNAME` (the account to grant) and `SystemRoot` (where
/// `icacls.exe` is resolved from).
///
/// Some tests mutate those variables process-wide to prove errors propagate;
/// others spawn the real `icacls`, which reads both. `cargo test` runs them in
/// parallel threads of one process, so an unguarded spawner can catch a
/// mutator's environment mid-flight. On CI that surfaced as `ensure_private`
/// failing with "The system cannot find the path specified": `SystemRoot`
/// momentarily pointed at `C:\definitely-not-windows` while another test was
/// proving that a missing `icacls` is reported.
///
/// `temp_env` locks against its own calls, not against a test that reads the
/// variable directly, so both sides have to take *this* guard. Same shape as
/// the credential-store lesson: two guards are not a guard.
///
/// `pub(crate)` because the `perms` tests reach the same spawn through the
/// public API and race identically.
///
/// Taken with `.expect`, not `unwrap_or_else(|e| e.into_inner())`: the
/// recovery closure is a function that never runs while the tests pass, and
/// the coverage gate reads that as an uncovered region. A poisoned guard only
/// happens when a test holding it has already failed, and failing the others
/// with "poisoned" alongside it is no loss.
/// This machine's hostname, or `None` when the OS declines to give one.
///
/// `COMPUTERNAME` rather than `GetComputerNameW`: Windows sets it for every
/// process, and reaching the API directly would mean raw FFI in a workspace that
/// forbids `unsafe`. The Unix side cannot use its environment equivalent for the
/// opposite reason - `$HOSTNAME` there is a shell variable that is not exported.
pub(crate) fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|h| !h.trim().is_empty())
}

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arguments_drop_inheritance_and_replace_the_grant() {
        let args = icacls_args(Path::new(r"C:\Users\u\.leviath\config.toml"), "u");
        assert_eq!(
            args,
            vec![
                r"C:\Users\u\.leviath\config.toml".to_string(),
                "/inheritance:r".to_string(),
                "/grant:r".to_string(),
                "u:(F)".to_string(),
            ]
        );
    }

    /// `PATH` is not consulted: a planted `icacls.exe` would otherwise run in
    /// place of the real one, during the step meant to be securing a secret.
    #[test]
    fn icacls_comes_from_the_system_directory() {
        let _env = ENV_LOCK.lock().expect("env lock");
        temp_env::with_var("SystemRoot", Some(r"C:\Windows"), || {
            assert_eq!(
                icacls_program(),
                PathBuf::from(r"C:\Windows\System32\icacls.exe")
            );
        });
        temp_env::with_var_unset("SystemRoot", || {
            assert_eq!(icacls_program(), PathBuf::from("icacls.exe"));
        });
    }

    #[test]
    fn a_missing_username_is_an_explained_error() {
        assert_eq!(resolve_user(Some("gerald".into())).unwrap(), "gerald");
        for absent in [None, Some(String::new()), Some("   ".to_string())] {
            let err = resolve_user(absent).expect_err("no account, no grant");
            assert_eq!(err.kind(), io::ErrorKind::NotFound);
            assert!(err.to_string().contains("USERNAME"), "{err}");
        }
    }

    #[test]
    fn a_failed_icacls_names_the_file_and_the_reason() {
        assert!(interpret_icacls(true, b"", Path::new("x")).is_ok());
        let err = interpret_icacls(false, b"Access is denied.\r\n", Path::new(r"C:\secret"))
            .expect_err("a non-zero exit is a failure");
        assert!(err.to_string().contains(r"C:\secret"), "{err}");
        assert!(err.to_string().contains("Access is denied."), "{err}");
    }

    /// The real thing, against a real file on a real Windows filesystem: after
    /// restricting, no *other user* may reach it.
    ///
    /// The property asserted is deliberately "no broad principal", not "exactly
    /// one account". `NT AUTHORITY\SYSTEM` and `BUILTIN\Administrators` survive
    /// on Windows and asserting otherwise would be asserting something the OS
    /// does not do - SYSTEM is the operating system and an administrator can
    /// take ownership of any file regardless, so neither is an escalation. What
    /// must not survive is a grant to `Users`, `Everyone` or
    /// `Authenticated Users`, which is what lets the account next door read
    /// your API keys.
    #[test]
    fn restricting_a_real_file_keeps_other_users_out() {
        let _env = ENV_LOCK.lock().expect("env lock");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::fs::write(&path, b"the key").unwrap();

        restrict_to_owner(&path).expect("restricting a file we own succeeds");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"the key",
            "still ours to read"
        );

        // Ask Windows what the ACL now says.
        let shown = std::process::Command::new(icacls_program())
            .arg(path.display().to_string())
            .output()
            .expect("icacls runs");
        let acl = String::from_utf8_lossy(&shown.stdout).into_owned();

        let user = std::env::var("USERNAME").unwrap();
        assert!(acl.contains(&user), "the owner keeps access:\n{acl}");
        for broad in ["\\Users:", "Everyone:", "Authenticated Users:"] {
            assert!(
                !acl.contains(broad),
                "'{broad}' must not be granted after restricting:\n{acl}"
            );
        }
    }

    /// With no account to grant, restricting must fail rather than quietly
    /// leaving the file as it was - the whole point is not to believe a secret
    /// is protected when it is not.
    #[test]
    fn restricting_fails_when_there_is_no_account_to_grant() {
        let _env = ENV_LOCK.lock().expect("env lock");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::fs::write(&path, b"x").unwrap();

        temp_env::with_var_unset("USERNAME", || {
            assert!(restrict_to_owner(&path).is_err(), "no USERNAME, no grant");
            // And it propagates through the public entry points rather than
            // being swallowed by either.
            assert!(ensure_private(&path, 0o600).is_err());
            assert!(write_with_mode(&path, b"x", 0o600).is_err());
        });
    }

    /// A missing `icacls` is reported too. Spawning is the one step that can
    /// fail before the tool ever runs, and treating it as success would mean
    /// silently skipping the hardening.
    #[test]
    fn a_missing_icacls_is_reported() {
        let _env = ENV_LOCK.lock().expect("env lock");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::fs::write(&path, b"x").unwrap();

        temp_env::with_var("SystemRoot", Some(r"C:\definitely-not-windows"), || {
            assert!(
                restrict_to_owner(&path).is_err(),
                "a system root with no icacls.exe is an error"
            );
        });
    }

    #[test]
    fn write_with_mode_writes_and_restricts() {
        let _env = ENV_LOCK.lock().expect("env lock");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        write_with_mode(&path, b"the key", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"the key");

        // And overwriting keeps it restricted.
        write_with_mode(&path, b"rotated", 0o600).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"rotated");
    }

    /// `write_atomic` only ever hands this a file it just created, so the
    /// write failure is reachable here alone.
    #[test]
    fn write_with_mode_reports_a_path_it_cannot_open() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing").join("f");
        assert!(write_with_mode(&missing, b"x", 0o600).is_err());
    }

    #[test]
    fn ensure_private_restricts_an_existing_file_and_skips_a_missing_one() {
        let _env = ENV_LOCK.lock().expect("env lock");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg");
        std::fs::write(&path, b"x").unwrap();
        assert_eq!(ensure_private(&path, 0o600).unwrap(), None);
        assert_eq!(
            ensure_private(&dir.path().join("nope"), 0o600).unwrap(),
            None
        );
    }

    #[test]
    fn set_mode_restricts_a_directory() {
        let _env = ENV_LOCK.lock().expect("env lock");
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("d");
        std::fs::create_dir(&sub).unwrap();
        set_mode(&sub, 0o700).unwrap();
        // Still ours to use.
        std::fs::write(sub.join("f"), b"x").unwrap();
    }

    #[test]
    fn set_mode_reports_a_missing_path() {
        let _env = ENV_LOCK.lock().expect("env lock");
        let dir = tempfile::tempdir().unwrap();
        assert!(set_mode(&dir.path().join("nope"), 0o600).is_err());
    }

    #[test]
    fn the_no_op_shims_answer() {
        let mut cmd = std::process::Command::new("cmd");
        configure_detached(&mut cmd);
        assert_eq!(current_uid(), 0);
        assert!(kill_process_group(1).is_ok());
    }

    /// The flag must hide the window without costing us the output - the whole
    /// point is that a piped child behaves exactly as before, just unseen.
    /// There is no way to observe "no window appeared" from a headless runner,
    /// so what is asserted here is the half that *can* regress.
    #[test]
    fn a_hidden_child_still_has_its_output_captured() {
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "echo leviath"]);
        hide_console_window(&mut cmd);
        let output = cmd.output().expect("cmd.exe runs");
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("leviath"),
            "stdout is still captured with CREATE_NO_WINDOW set"
        );
    }
}
