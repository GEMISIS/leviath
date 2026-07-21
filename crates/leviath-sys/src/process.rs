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
}
