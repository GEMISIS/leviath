//! Process control: detached spawning and signalling.

use std::process::Command;

/// Configure `cmd` so the spawned child detaches into its own session,
/// surviving the terminal that launched it.
///
/// On Unix this installs a `pre_exec` hook that calls `setsid()` in the forked
/// child just before `exec`. On non-Unix platforms it is a no-op.
///
/// Call this before `cmd.spawn()`.
pub fn configure_detached(cmd: &mut Command) {
    crate::platform::configure_detached(cmd);
}

/// Send `SIGTERM` to the process with the given pid, requesting graceful
/// shutdown.
///
/// A pid of `0` is rejected (returning `false`) rather than signalling the
/// caller's whole process group — `kill(0, …)` targets the current process
/// group, which is never what a caller passing a worker pid intends. On
/// non-Unix platforms this is always a no-op returning `false`.
///
/// Returns whether the signal was dispatched. Any error from the underlying
/// `kill(2)` (e.g. `ESRCH` when the process has already exited) is ignored —
/// the caller's intent is best-effort termination.
pub fn terminate(pid: u32) -> bool {
    crate::platform::terminate(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configure_detached_public_wrapper_registers_without_spawning() {
        // Exercises the public `process::configure_detached` shim (distinct
        // from the platform impl its own tests cover); registering the hook
        // must not fork/exec or panic.
        let mut cmd = Command::new("true");
        configure_detached(&mut cmd);
    }

    #[test]
    fn terminate_public_wrapper_rejects_pid_zero() {
        assert!(!terminate(0));
    }

    #[test]
    fn terminate_public_wrapper_dispatches_to_nonexistent_pid() {
        // A pid far beyond any real PID_MAX: on Unix `kill` returns ESRCH
        // (ignored → true); on non-Unix the platform no-op returns false.
        let dispatched = terminate(2_000_000_000);
        assert_eq!(dispatched, cfg!(unix));
    }
}
