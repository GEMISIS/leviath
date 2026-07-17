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
