//! Deciding what `lev daemon start` / `stop` / `status` should do.
//!
//! Each function here is the *judgement* part of a subcommand: given what was
//! observed about the daemon, what has to happen and what gets printed. The
//! observing is left to the caller, for the reason
//! [`readiness::poll_until`](super::readiness::poll_until) takes its predicate
//! as an argument - the sequencing is worth testing, and only the probe needs a
//! real socket or a real process.

/// What has to happen before a daemon on the current build is running.
///
/// Returned rather than performed, so `main.rs` supplies the socket probe and
/// the process spawn while the decision between these three cases stays here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartSteps {
    /// Shut down what is running before spawning.
    ///
    /// Only ever true alongside `spawn`: the reason to stop a daemon here is to
    /// replace it, never to leave the machine without one.
    pub shutdown_first: bool,
    /// Spawn a daemon.
    pub spawn: bool,
}

impl StartSteps {
    /// Nothing to do; one is already up on this build.
    const SATISFIED: Self = Self {
        shutdown_first: false,
        spawn: false,
    };
}

/// What `lev` must do to reach "a daemon on the current build is running".
///
/// A daemon on an older build is restarted rather than reused. It cannot pick
/// up new code, and the alternative - talking to it anyway - means a `lev` that
/// was just rebuilt silently drives the previous build's engine. The restart is
/// safe because the daemon reloads its persisted agents on startup, so
/// in-flight runs survive the swap.
///
/// `stale` is only meaningful when `running`; a build marker left by a daemon
/// that has since exited says nothing about what is about to be spawned.
pub fn start_steps(running: bool, stale: bool) -> StartSteps {
    match (running, stale) {
        (true, false) => StartSteps::SATISFIED,
        (true, true) => StartSteps {
            shutdown_first: true,
            spawn: true,
        },
        (false, _) => StartSteps {
            shutdown_first: false,
            spawn: true,
        },
    }
}

/// What to do when the control channel would not accept a shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopFallback {
    /// Signal the recorded process group instead.
    ///
    /// Without this a daemon that cannot be talked to cannot be stopped either,
    /// and `lev daemon restart` - which stops before it starts - could never
    /// recover one that had wedged.
    Signal(u32),
    /// Nothing recorded a pid, so the control-channel error is the whole story
    /// and is reported as-is rather than replaced by a vaguer one.
    Propagate,
}

/// Decide the fallback for a refused shutdown, given the recorded pid.
pub fn stop_fallback(recorded_pid: Option<u32>) -> StopFallback {
    match recorded_pid {
        Some(pid) => StopFallback::Signal(pid),
        None => StopFallback::Propagate,
    }
}

/// The line `lev daemon stop` prints, or the error it fails with.
///
/// What `lev daemon stop` prints when the daemon ignores it for the whole of
/// [`super::readiness::READY_TIMEOUT`].
///
/// Spelled per platform because the timeout is, and a `&'static str` cannot be
/// formatted at runtime; `the_shutdown_message_names_the_real_timeout` holds
/// the two in step.
const SHUTDOWN_TIMEOUT_MSG: &str = match cfg!(windows) {
    true => "the leviath daemon did not shut down within 15s",
    false => "the leviath daemon did not shut down within 5s",
};

/// `was_running` and `stopped` are separate observations because they answer
/// different questions and the pair "nothing was running" and "it did not stop"
/// must not read the same: the first is a success.
pub fn stop_outcome(was_running: bool, stopped: bool) -> Result<&'static str, &'static str> {
    match (was_running, stopped) {
        (false, _) => Ok("daemon not running"),
        (true, true) => Ok("daemon stopped"),
        (true, false) => Err(SHUTDOWN_TIMEOUT_MSG),
    }
}

/// The lines `lev daemon status` prints.
///
/// `supervision` is `None` on a platform with no supported supervisor, where
/// there is genuinely nothing to report - as distinct from a supported platform
/// with nothing installed, which says so.
pub fn status_lines(running: bool, agents: usize, supervision: Option<String>) -> Vec<String> {
    let mut lines = vec![crate::commands::daemon::format_status(running, agents)];
    lines.extend(supervision);
    lines
}

#[cfg(test)]
mod tests;
