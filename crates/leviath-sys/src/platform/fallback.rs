//! Implementations for targets that are neither Unix nor Windows.
//!
//! There is no permission model to apply here, so the hardening calls are
//! no-ops that succeed. That is a real gap on such a target - secrets are
//! written with whatever protection the platform gives by default - and it is
//! stated rather than papered over. Unix uses POSIX modes and Windows uses
//! ACLs; both are implemented in their own modules.

use std::io;
use std::path::Path;

pub(crate) fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// A plain write: this platform has no permission model to apply, and faking
/// one here would claim a protection that is not being applied.
pub(crate) fn write_with_mode(path: &Path, contents: &[u8], _mode: u32) -> io::Result<()> {
    std::fs::write(path, contents)
}

/// A plain append-open, for the same reason `write_with_mode` is a plain write.
pub(crate) fn open_append_with_mode(path: &Path, _mode: u32) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// A plain recursive create: no permission model to apply here either.
pub(crate) fn create_dir_all_with_mode(path: &Path, _mode: u32) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

pub(crate) fn ensure_private(_path: &Path, _mode: u32) -> io::Result<Option<u32>> {
    Ok(None)
}

pub(crate) fn configure_detached(_cmd: &mut std::process::Command) {}

/// Nothing to detach from on such a target, so this is a plain spawn.
pub(crate) fn spawn_detached(
    cmd: &mut std::process::Command,
) -> std::io::Result<std::process::Child> {
    cmd.spawn()
}

/// No consoles to suppress on such a target; Windows is the only platform where
/// a child process can be handed a window.
pub(crate) fn hide_console_window(_cmd: &mut std::process::Command) {}

/// No POSIX uid here; the value is only used to address a per-user
/// launchd/systemd domain, neither of which exists on such a target.
pub(crate) fn current_uid() -> u32 {
    0
}

/// No process groups to signal on this platform; killing the direct child is
/// all that is available (and is what the caller already does).
pub(crate) fn kill_process_group(_pgid: u32) -> io::Result<()> {
    Ok(())
}

/// No hostname API on such a target, so the caller reports that it is unknown
/// rather than inventing a name for the machine.
pub(crate) fn hostname() -> Option<String> {
    None
}

/// Nothing to tie a command to on such a target: the guard still kills the
/// direct child when it is dropped, and that is all this platform offers.
pub(crate) struct Tie;

pub(crate) fn tie_child(_child: &tokio::process::Child) -> Option<Tie> {
    None
}

/// No way to ask when a process started, so every record a crashed daemon left
/// reads as gone and nothing is killed on a guess.
pub(crate) fn process_started_ms(_pid: u32) -> Option<u64> {
    None
}

pub(crate) fn process_group_of(_pid: u32) -> Option<u32> {
    None
}

pub(crate) fn kill_tree(_pid: u32, _pgid: u32) -> io::Result<()> {
    Ok(())
}
