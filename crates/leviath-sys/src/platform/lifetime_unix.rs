//! Unix halves of [`crate::lifetime`]: the watchdog that ties a command's
//! process group to this process, and the questions the start-up sweep asks
//! about a pid a crashed daemon left behind.
//!
//! # Why a watchdog and not `PR_SET_PDEATHSIG`
//!
//! Linux can ask the kernel to signal a child when its parent dies, but only
//! from inside the child between `fork` and `exec`, which means a `pre_exec`
//! hook, and that is `unsafe` in a workspace that forbids it. It would also be
//! the wrong tool here twice over: the signal fires when the *thread* that
//! spawned the child exits, and tokio retires idle blocking threads, so a long
//! command would be killed while the daemon was still alive; and it reaches the
//! shell alone, not the `sleep` the shell started. macOS has no equivalent at
//! all.
//!
//! The watchdog works the same way on both. It is a `/bin/sh` that blocks
//! reading a pipe whose only writer is this process. The kernel closes that
//! writer however this process ends, `kill -9` included, so the read returns
//! and the watchdog kills the whole group.

use std::io;
use std::process::{Child, Command, Stdio};

/// The watchdog's script. `$1` is the process group to kill.
///
/// The signals a terminal or a service manager sends to a group are ignored, so
/// a Ctrl-C meant for a foreground daemon cannot take the watchdog down before
/// it has done its job. `read` returns on end of file, which is the only thing
/// that ever arrives: nothing writes to the pipe.
const WATCHDOG: &str = "trap '' HUP INT QUIT TERM; read -r line; kill -KILL -- -\"$1\" 2>/dev/null";

/// A running watchdog for one command's process group.
///
/// Dropped when the command is done with: the watchdog is killed and reaped
/// first, so closing the pipe afterwards does not make it fire at a group that
/// has already been dealt with.
pub(crate) struct Tie {
    watchdog: Child,
}

impl Drop for Tie {
    fn drop(&mut self) {
        // Both fail only when the watchdog has already gone, which leaves
        // nothing to do.
        let _ = self.watchdog.kill();
        let _ = self.watchdog.wait();
    }
}

/// Tie the process group `child` leads to this process. `None` for a child
/// already reaped, which has nothing left to tie.
pub(crate) fn tie_child(child: &tokio::process::Child) -> Option<Tie> {
    child.id().and_then(tie)
}

/// Start a watchdog for the process group `pgid`.
///
/// `None` when it cannot be started, which costs only this layer: the record
/// the start-up sweep reads still covers the command.
fn tie(pgid: u32) -> Option<Tie> {
    tie_with(Command::new("/bin/sh"), pgid)
}

/// [`tie`], with the shell injected so the arm where it cannot start is
/// reachable from a test.
fn tie_with(mut cmd: Command, pgid: u32) -> Option<Tie> {
    use std::os::unix::process::CommandExt;
    cmd.args(["-c", WATCHDOG, "leviath-watchdog", &pgid.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .current_dir("/")
        // `read` and `kill` are shell builtins, so it needs none of the
        // daemon's environment, credentials included.
        .env_clear()
        // Its own group, so a signal sent to the daemon's group (a terminal's
        // Ctrl-C) does not reach it.
        .process_group(0);
    match cmd.spawn() {
        Ok(watchdog) => Some(Tie { watchdog }),
        Err(e) => {
            tracing::warn!(pgid, error = %e, "could not start the watchdog that stops this command if the daemon dies");
            None
        }
    }
}

/// When the process `pid` started, in milliseconds since the Unix epoch, or
/// `None` when there is no such process (or `ps` could not say).
///
/// `ps -o etime=` is the one spelling macOS and Linux share. Nothing in `std`
/// or `nix` answers this on macOS without `unsafe`, and the question is asked
/// only for the records a crashed daemon left, so a process per record is
/// cheap.
pub(crate) fn process_started_ms(pid: u32) -> Option<u64> {
    // One chain: `ps` failing to start at all is not something a test can
    // arrange, and it reads the same as a pid with no process.
    Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|output| parse_etime(&String::from_utf8_lossy(&output.stdout)))
        .map(|elapsed| crate::lifetime::now_ms().saturating_sub(elapsed * 1000))
}

/// Parse the `[[dd-]hh:]mm:ss` elapsed time `ps -o etime=` prints into
/// seconds. `None` for anything else, including the empty output `ps` gives
/// for a pid with no process.
fn parse_etime(text: &str) -> Option<u64> {
    let text = text.trim();
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, text),
    };
    let mut seconds = 0u64;
    let mut fields = 0;
    for field in clock.split(':') {
        seconds = seconds * 60 + field.parse::<u64>().ok()?;
        fields += 1;
    }
    match fields {
        2 | 3 => Some(days * 86_400 + seconds),
        _ => None,
    }
}

/// The process group `pid` belongs to, or `None` when there is no such process.
pub(crate) fn process_group_of(pid: u32) -> Option<u32> {
    nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(pid as i32)))
        .ok()
        .map(|pgid| pgid.as_raw() as u32)
}

/// Kill a leftover command: its whole process group.
pub(crate) fn kill_tree(_pid: u32, pgid: u32) -> io::Result<()> {
    super::kill_process_group(pgid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::time::Duration;

    /// A `sleep` leading its own process group, the way the shell tool starts
    /// a command.
    fn grouped_sleep() -> Child {
        Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("sleep starts")
    }

    /// Whether `child` exits within a few seconds. Waited on from a thread so
    /// a child that never exits fails the test rather than hanging it.
    fn exits_soon(mut child: Child) -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(child.wait()));
        rx.recv_timeout(Duration::from_secs(10)).is_ok()
    }

    /// What a crash looks like from the watchdog's side: the pipe closes with
    /// the watchdog still running, and the group goes with it.
    #[test]
    fn a_closed_pipe_kills_the_group() {
        let command = grouped_sleep();
        let mut tie = tie(command.id()).expect("the watchdog starts");
        // The daemon's end of the pipe, closed the way the kernel closes it
        // when the daemon dies.
        drop(tie.watchdog.stdin.take());
        assert!(exits_soon(command), "the watchdog killed the group");
        assert!(tie.watchdog.wait().is_ok(), "and then exited");
    }

    /// The ordinary end of a command: the watchdog is stopped before the pipe
    /// closes, so the group it names is never signalled.
    #[test]
    fn dropping_the_tie_stops_the_watchdog_without_firing() {
        let mut command = grouped_sleep();
        drop(tie(command.id()).expect("the watchdog starts"));
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            command.try_wait().expect("waitable").is_none(),
            "the command is still running"
        );
        command.kill().expect("ours to kill");
        command.wait().expect("reaped");
    }

    #[test]
    fn etime_reads_every_shape_ps_prints() {
        assert_eq!(parse_etime("   00:07\n"), Some(7));
        assert_eq!(parse_etime("01:02"), Some(62));
        assert_eq!(parse_etime("01:00:05"), Some(3_605));
        assert_eq!(parse_etime("2-00:00:01"), Some(2 * 86_400 + 1));
        for bad in ["", "7", "1:2:3:4", "x-00:01", "00:xx"] {
            assert_eq!(parse_etime(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_shell_that_cannot_start_is_no_tie() {
        assert!(tie_with(Command::new("/nonexistent/sh"), 1).is_none());
    }

    #[test]
    fn a_running_process_reports_when_it_started() {
        let mut command = grouped_sleep();
        let started = process_started_ms(command.id()).expect("ps answers");
        // Within the second `etime` counts in, plus slack for a loaded machine.
        assert!(crate::lifetime::now_ms().abs_diff(started) < 5_000);
        assert_eq!(process_group_of(command.id()), Some(command.id()));
        command.kill().expect("ours to kill");
        command.wait().expect("reaped");
        assert_eq!(process_started_ms(command.id()), None, "gone once reaped");
        assert_eq!(process_group_of(command.id()), None);
    }

    #[test]
    fn kill_tree_kills_the_group() {
        let command = grouped_sleep();
        kill_tree(command.id(), command.id()).expect("ours to kill");
        assert!(exits_soon(command));
    }
}
