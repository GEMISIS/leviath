//! Windows halves of [`crate::lifetime`]: a kill-on-close job object per
//! command, and the questions the start-up sweep asks about a pid a crashed
//! daemon left behind.
//!
//! A job object created with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` terminates
//! every process in it when its last handle closes. The daemon holds the only
//! handle (it is not inheritable, so no child keeps the job alive), and the
//! kernel closes it however the daemon ends, including `taskkill /F`. The
//! command's own children are started inside the job and go with it, which a
//! plain kill of `cmd.exe` never reached.
//!
//! `win32job` holds the FFI behind a safe API, so this crate stays free of
//! `unsafe`.

use std::io;
use std::process::Stdio;

use win32job::{ExtendedLimitInfo, Job};

/// One command's kill-on-close job. Dropping it closes the handle, which ends
/// whatever is still running in the job.
pub(crate) struct Tie {
    _job: Job,
}

/// Put `child` in a fresh kill-on-close job. `None` for a child already reaped.
pub(crate) fn tie_child(child: &tokio::process::Child) -> Option<Tie> {
    child.raw_handle().and_then(|handle| tie(handle as isize))
}

/// Put the process behind `handle` in a fresh kill-on-close job.
///
/// `None` when the job cannot be made or the process cannot join it (an older
/// host whose own job forbids nesting). That costs only this layer: the record
/// the start-up sweep reads still covers the command.
fn tie(handle: isize) -> Option<Tie> {
    let mut info = ExtendedLimitInfo::new();
    info.limit_kill_on_job_close();
    match Job::create_with_limit_info(&info)
        .and_then(|job| job.assign_process(handle).map(|()| job))
    {
        Ok(job) => Some(Tie { _job: job }),
        Err(e) => {
            tracing::warn!(error = %e, "could not tie this command to the daemon with a job object");
            None
        }
    }
}

/// When the process `pid` started, in milliseconds since the Unix epoch, or
/// `None` when there is no such process.
///
/// PowerShell is slow to start, but this is asked only for the records a
/// crashed daemon left, and reading another process's start time any other
/// way means raw FFI.
pub(crate) fn process_started_ms(pid: u32) -> Option<u64> {
    let script = format!(
        "[DateTimeOffset]::new((Get-Process -Id {pid} -ErrorAction Stop).StartTime).ToUnixTimeMilliseconds()"
    );
    let mut cmd = std::process::Command::new(super::system32_program(
        r"WindowsPowerShell\v1.0\powershell.exe",
    ));
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    super::hide_console_window(&mut cmd);
    // One chain: PowerShell failing to start at all is not something a test
    // can arrange, and it reads the same as a pid with no process.
    cmd.output()
        .ok()
        .and_then(|output| String::from_utf8_lossy(&output.stdout).trim().parse().ok())
}

/// Windows has no process groups: a command is named by its own pid, which
/// the start-time check has already shown is alive.
pub(crate) fn process_group_of(pid: u32) -> Option<u32> {
    Some(pid)
}

/// Kill a leftover command and everything it started.
pub(crate) fn kill_tree(pid: u32, _pgid: u32) -> io::Result<()> {
    let mut cmd = std::process::Command::new(super::system32_program("taskkill.exe"));
    cmd.args(["/F", "/T", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    super::hide_console_window(&mut cmd);
    cmd.status().and_then(|status| match status.success() {
        true => Ok(()),
        false => Err(io::Error::other(format!("taskkill exited with {status}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use std::process::{Child, Command};
    use std::time::Duration;

    /// A `cmd.exe` running `ping`, so there is a child of the child to stop.
    fn pinging() -> Child {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", "ping -n 60 127.0.0.1 >NUL"])
            .stdout(Stdio::null());
        super::super::hide_console_window(&mut cmd);
        cmd.spawn().expect("cmd starts")
    }

    /// Whether `child` exits within a few seconds, handing it back so its
    /// handle stays open (and its pid reserved) until the caller drops it.
    fn exits_soon(mut child: Child) -> Option<Child> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = child.wait();
            tx.send(child)
        });
        rx.recv_timeout(Duration::from_secs(20)).ok()
    }

    /// Closing the job is what a crash does to it, and what dropping the tie
    /// does on purpose.
    #[test]
    fn closing_the_job_ends_the_command() {
        let command = pinging();
        let tie = tie(command.as_raw_handle() as isize).expect("the job takes the command");
        drop(tie);
        assert!(
            exits_soon(command).is_some(),
            "the job took the command down"
        );
    }

    #[test]
    fn a_handle_that_cannot_join_is_no_tie() {
        assert!(tie(0).is_none());
    }

    #[test]
    fn a_running_process_reports_when_it_started() {
        let _env = super::super::ENV_LOCK.lock().expect("env lock");
        let command = pinging();
        let pid = command.id();
        let started = process_started_ms(pid).expect("PowerShell answers");
        assert!(crate::lifetime::now_ms().abs_diff(started) < 30_000);
        assert_eq!(process_group_of(pid), Some(pid));
        kill_tree(pid, pid).expect("ours to kill");
        // Our handle keeps the exited process's pid reserved until it closes.
        drop(exits_soon(command).expect("taskkill stopped it"));
        assert_eq!(process_started_ms(pid), None, "gone once killed");
        assert!(kill_tree(pid, pid).is_err(), "nothing left to kill");
    }
}
