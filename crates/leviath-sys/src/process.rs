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
