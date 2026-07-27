//! Process control: detached spawning.

use std::process::Command;

/// Configure `cmd` so the spawned child detaches into its own process group,
/// surviving the terminal that launched it.
///
/// On Unix this uses the safe, stable `Command::process_group(0)`; on non-Unix
/// platforms it is a no-op. Call this before `cmd.spawn()`.
pub fn configure_detached(cmd: &mut Command) {
    crate::platform::configure_detached(cmd);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_uid_is_reported() {
        // Just has to answer without panicking; root (0) is a legitimate value
        // on Unix and the only value on non-Unix.
        let _uid: u32 = current_uid();
    }

    #[test]
    fn configure_detached_public_wrapper_registers_without_spawning() {
        // Exercises the public `process::configure_detached` shim (distinct
        // from the platform impl its own tests cover); registering the hook
        // must not fork/exec or panic.
        let mut cmd = Command::new("true");
        configure_detached(&mut cmd);
    }
}
