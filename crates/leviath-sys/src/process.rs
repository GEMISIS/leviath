//! Process control: detached spawning and console-window suppression.

use std::ffi::OsStr;
use std::process::Command;

/// Configure `cmd` so the spawned child detaches into its own process group,
/// surviving the terminal that launched it.
///
/// On Unix this uses the safe, stable `Command::process_group(0)`; on non-Unix
/// platforms it is a no-op. Call this before `cmd.spawn()`.
pub fn configure_detached(cmd: &mut Command) {
    crate::platform::configure_detached(cmd);
}

/// Configure `cmd` so the spawned child gets no console window.
///
/// On Windows a console application is given a console. A child of a process
/// that has one shares it and draws nothing; a child of a process that has
/// none, such as the daemon started from Explorer, from a service, or from a UI
/// console, gets a brand new window on the interactive desktop. Agent tooling
/// spawns `cmd.exe` many times per run, so without this the desktop fills with
/// flashing consoles (issue #228). Elsewhere this is a no-op: no other platform
/// hands a child a window.
///
/// Apply it to a child whose stdout/stderr are already piped or nulled, which
/// is what every caller in this workspace does. Do **not** apply it to a child
/// meant to share the user's terminal: the editor launched by `lev run` needs
/// that console to draw in, and starting `vim` without one just breaks it.
///
/// One sharp edge worth knowing: `Command::creation_flags` *assigns* the flag
/// word rather than OR-ing into it, so two callers setting different flags on
/// one command would silently clobber each other. This function is deliberately
/// the only writer of creation flags in the workspace. Add any future flag
/// here, alongside `CREATE_NO_WINDOW`, rather than at a call site.
pub fn hide_console_window(cmd: &mut Command) {
    crate::platform::hide_console_window(cmd);
}

/// A [`Command`] for a child that must not take a console window.
///
/// **This is where the decision is made.** Hiding used to be a second call the
/// caller made after building the command, at seven sites across five crates,
/// and a new spawn site that forgot it looked exactly like one that did not
/// need it. Making it part of construction means the only way to get a child
/// process is to have already answered the question.
///
/// The counterpart is [`terminal_command`], for the single child that is meant
/// to be seen. There is no third option on purpose: a `Command::new` outside
/// this module is rejected by `.sgrules/no-raw-command-new.yml`.
pub fn child_command(program: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(program);
    hide_console_window(&mut cmd);
    cmd
}

/// The tokio twin of [`child_command`].
///
/// `tokio::process::Command` wraps a `std::process::Command`, so `as_std_mut`
/// reaches the one the flag is written on and both flavours share a single
/// implementation rather than a second copy of the `#[cfg]`.
pub fn child_command_async(program: impl AsRef<OsStr>) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    hide_console_window(cmd.as_std_mut());
    cmd
}

/// A [`Command`] for a child that *must* inherit the user's terminal.
///
/// The editor `lev run` opens is the only one: it draws in the terminal it
/// inherits, and starting `vim` without a console leaves it nowhere to draw and
/// nothing to read from. Named rather than reached for with a bare
/// `Command::new` so the exception is a decision in the source instead of an
/// omission, and so the lint has something to point at.
pub fn terminal_command(program: impl AsRef<OsStr>) -> Command {
    Command::new(program)
}

/// SIGKILL every process in the group led by `pgid` (a no-op on platforms
/// without process groups).
///
/// Killing a child shell is not enough to stop what it started: the shell's own
/// children are reparented to init and keep running. A cancelled agent's
/// `sleep 400` outliving the run that started it is exactly that. Spawning the
/// shell into its own group (via [`configure_detached`]) and signalling the
/// group tears down the whole tree.
///
/// Errors are the ordinary case (the group already exited) and are the caller's
/// to ignore.
pub fn kill_process_group(pgid: u32) -> std::io::Result<()> {
    crate::platform::kill_process_group(pgid)
}

/// The calling user's numeric id.
///
/// Used to address a per-user service domain (`launchctl bootstrap gui/<uid>`).
/// Returns `0` on platforms with no POSIX uid, where no such domain exists.
pub fn current_uid() -> u32 {
    crate::platform::current_uid()
}

/// The effective uid of the process on the other end of a connected
/// Unix-domain socket.
///
/// The kernel's answer, not the peer's claim, so it cannot be spoofed by
/// anything the peer sends. This is what makes the daemon's control socket
/// safe: file permissions on a Unix socket are advisory on macOS and the BSDs
/// (the mode is not consulted on `connect`), so the mode alone was never the
/// guarantee it was documented to be.
///
/// `None` when the platform cannot report it - which the caller must treat as
/// "refuse", since an unidentifiable peer is not an authorized one.
///
/// Unix-only: on Windows the control channel is not a Unix socket, so there is
/// no fd to interrogate and no caller for this.
#[cfg(unix)]
pub fn peer_uid(sock: &impl std::os::fd::AsFd) -> Option<u32> {
    crate::platform::peer_uid(sock)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The check that makes the daemon's control socket safe: the peer's uid
    /// comes from the kernel, not from anything the peer claims. A socket we
    /// connected to ourselves must report our own uid.
    #[cfg(unix)]
    #[test]
    fn peer_uid_reports_our_own_uid_for_a_self_connection() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let client = std::os::unix::net::UnixStream::connect(&path).expect("connect");
        let (server, _) = listener.accept().expect("accept");

        // Both ends agree, and both agree with the process's own uid - a peer
        // check that returned `None` here would refuse every legitimate
        // connection, which is the failure mode worth catching.
        assert_eq!(peer_uid(&server), Some(current_uid()));
        assert_eq!(peer_uid(&client), Some(current_uid()));
    }

    #[test]
    fn current_uid_is_reported() {
        // Just has to answer without panicking; root (0) is a legitimate value
        // on Unix and the only value on non-Unix.
        let _uid: u32 = current_uid();
    }

    #[test]
    fn kill_process_group_public_wrapper_reports_a_missing_group() {
        // Exercises the public shim (distinct from the platform impl its own
        // tests cover). A group that cannot exist errors on Unix and is a no-op
        // where the platform has no process groups - both are outcomes the
        // caller ignores, so either result is acceptable here.
        let _ = kill_process_group(0x7FFF_FFFF);
    }

    #[test]
    fn configure_detached_public_wrapper_registers_without_spawning() {
        // Exercises the public `process::configure_detached` shim (distinct
        // from the platform impl its own tests cover); registering the hook
        // must not fork/exec or panic.
        let mut cmd = Command::new("true");
        configure_detached(&mut cmd);
    }

    #[test]
    fn hide_console_window_public_wrapper_registers_without_spawning() {
        // Same shape as the shim above: callers apply this unconditionally on
        // every platform, so it must configure and return rather than fail
        // where there is no window to suppress. Whether it took effect is only
        // observable on Windows, and is asserted in that platform module.
        let mut cmd = Command::new("true");
        hide_console_window(&mut cmd);
    }

    /// The constructors have to *be* the decision, not just offer it: a caller
    /// that builds through them gets a hidden console without a second call.
    /// The flag itself is only observable on Windows and is asserted in that
    /// platform module; here the point is that construction succeeds and the
    /// program survives, so a caller cannot be silently handed a broken command.
    #[test]
    fn child_command_builds_a_usable_command() {
        let cmd = child_command("true");
        assert_eq!(cmd.get_program(), "true");
    }

    #[test]
    fn child_command_async_builds_a_usable_command() {
        let cmd = child_command_async("true");
        assert_eq!(cmd.as_std().get_program(), "true");
    }

    /// The editor is the one child meant to be seen, so this deliberately does
    /// *not* hide the console - see `editor.rs`, its only caller.
    #[test]
    fn terminal_command_builds_a_usable_command() {
        let cmd = terminal_command("true");
        assert_eq!(cmd.get_program(), "true");
    }
}
