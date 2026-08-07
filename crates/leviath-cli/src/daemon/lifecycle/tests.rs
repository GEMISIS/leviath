//! Tests for the daemon-lifecycle decisions.

use super::*;

/// The case the stale check exists for. A `lev` that was just rebuilt must not
/// silently drive the previous build's engine, so a running-but-stale daemon is
/// replaced rather than reused.
#[test]
fn a_stale_daemon_is_replaced_rather_than_reused() {
    let steps = start_steps(true, true);
    assert!(steps.shutdown_first);
    assert!(steps.spawn);
}

/// The control, so the test above is not passing because everything restarts:
/// a current daemon is left alone, which is the common path and the one that
/// must cost nothing.
#[test]
fn a_current_daemon_is_left_alone() {
    assert_eq!(start_steps(true, false), StartSteps::SATISFIED);
}

/// Nothing running means spawn, and nothing to shut down first. The `stale`
/// reading is ignored: a build marker left by a daemon that has since exited
/// says nothing about the one about to be spawned.
#[test]
fn nothing_running_spawns_without_a_shutdown() {
    for stale in [true, false] {
        let steps = start_steps(false, stale);
        assert!(!steps.shutdown_first, "stale={stale}");
        assert!(steps.spawn, "stale={stale}");
    }
}

/// The invariant behind the pair: stopping is only ever a prelude to starting.
/// A combination that shut the daemon down and did not replace it would leave
/// the machine without one, which no caller of this asks for.
#[test]
fn a_shutdown_is_never_ordered_without_a_spawn() {
    for running in [true, false] {
        for stale in [true, false] {
            let steps = start_steps(running, stale);
            assert!(
                !steps.shutdown_first || steps.spawn,
                "running={running} stale={stale} would stop without starting"
            );
        }
    }
}

#[test]
fn a_recorded_pid_is_signalled_and_a_missing_one_propagates() {
    assert_eq!(stop_fallback(Some(4321)), StopFallback::Signal(4321));
    assert_eq!(stop_fallback(None), StopFallback::Propagate);
}

/// "Nothing was running" is a success, and must not read like "it would not
/// stop" - which is the failure.
/// A platform with no supervisor prints one line, not a line saying nothing is
/// installed - those are different facts.
#[test]
fn status_omits_supervision_when_there_is_none_to_report() {
    assert_eq!(status_lines(true, 3, None).len(), 1);
    let with = status_lines(true, 3, Some("supervised".to_string()));
    assert_eq!(with.len(), 2);
    assert_eq!(with[1], "supervised");
}

#[test]
fn not_running_is_a_success_and_not_stopping_is_not() {
    assert_eq!(stop_outcome(false, false), Ok("daemon not running"));
    assert_eq!(stop_outcome(false, true), Ok("daemon not running"));
    assert_eq!(stop_outcome(true, true), Ok("daemon stopped"));
    assert!(stop_outcome(true, false).is_err());
}
