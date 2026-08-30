//! Waiting for the daemon to appear or go away.
//!
//! Both directions are the same shape: ask a cheap predicate whether the state
//! has flipped yet, and keep asking until it has or the wait is over. The
//! predicate is a socket probe, so this lives here rather than in `main.rs` -
//! the timing is a decision worth testing, and the probe is the only part that
//! needs a real socket.

use std::time::Duration;

/// How long to keep asking before giving up.
///
/// Windows gets longer. Starting the daemon there has to bind a named pipe and
/// detach into a job object, and under a supervisor opening many sessions at
/// once those serialise: the 5s that is generous on Unix is regularly missed
/// there, leaving sessions `runtime-missing` and the control pipe reporting
/// "All pipe instances are busy" (os error 231). The longer window is paid for
/// only by a start that would otherwise have failed - the poll returns as soon
/// as the daemon answers, and a healthy one answers in ~20ms on either
/// platform.
pub const READY_TIMEOUT: Duration = match cfg!(windows) {
    true => Duration::from_secs(15),
    false => Duration::from_secs(5),
};

/// First gap between polls.
const FIRST_DELAY: Duration = Duration::from_millis(2);

/// Ceiling the gap doubles up to.
const MAX_DELAY: Duration = Duration::from_millis(50);

/// Poll `done` until it returns true or [`READY_TIMEOUT`] elapses, starting at
/// 2ms and doubling to a 50ms ceiling. Returns whether it flipped in time.
///
/// The backoff is the point. The daemon boots in about 20ms, so a fixed 50ms
/// tick spends more time waiting than the daemon spends starting: it was most
/// of a measured 97ms cold `lev run`. Doubling to the same 50ms ceiling
/// leaves the slow path (a daemon that genuinely takes seconds) unchanged
/// while making the common path cost one 2ms sleep.
///
/// `done` is checked before the first sleep, so a predicate that is already
/// true costs nothing.
///
/// `&mut dyn FnMut` rather than `impl FnMut`: a generic parameter gives rustc
/// one monomorphization per call site and llvm-cov instruments each
/// separately - it reported 18 of 26 instantiations as 0-hit here even though
/// the union of them covers every line. A trait object is one instantiation
/// however many callers there are. Same reason `run/task.rs`'s
/// `resolve_task_with` takes one, and the cost is a vtable dispatch on a loop
/// that sleeps between iterations.
pub async fn poll_until(done: &mut dyn FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    let mut delay = FIRST_DELAY;
    while !done() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(MAX_DELAY);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// Drive the backoff against a virtual clock: `tokio::time::pause` makes
    /// `sleep` return as soon as nothing else can run, so a five-second
    /// timeout is tested without waiting five seconds.
    #[tokio::test(start_paused = true)]
    async fn an_already_true_predicate_returns_without_sleeping() {
        let started = tokio::time::Instant::now();
        assert!(poll_until(&mut || true).await);
        assert_eq!(tokio::time::Instant::now(), started, "it slept anyway");
    }

    #[tokio::test(start_paused = true)]
    async fn a_predicate_that_never_flips_gives_up_at_the_timeout() {
        let started = tokio::time::Instant::now();
        assert!(!poll_until(&mut || false).await);
        assert!(
            tokio::time::Instant::now() - started >= READY_TIMEOUT,
            "gave up early"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn it_returns_as_soon_as_the_predicate_flips() {
        let calls = Cell::new(0);
        assert!(
            poll_until(&mut || {
                calls.set(calls.get() + 1);
                calls.get() == 4
            })
            .await
        );
        assert_eq!(calls.get(), 4, "it kept polling after the flip");
    }

    #[tokio::test(start_paused = true)]
    async fn the_delay_doubles_and_then_holds_at_the_ceiling() {
        // Record the gap between polls by reading the virtual clock inside the
        // predicate. Asserting on the sequence is what pins the backoff; a
        // test that only checked the total would pass on a fixed tick.
        let gaps = std::cell::RefCell::new(Vec::new());
        let last = Cell::new(tokio::time::Instant::now());
        let calls = Cell::new(0);
        poll_until(&mut || {
            let now = tokio::time::Instant::now();
            gaps.borrow_mut().push(now - last.get());
            last.set(now);
            calls.set(calls.get() + 1);
            calls.get() == 8
        })
        .await;

        let gaps = gaps.borrow();
        // gaps[0] is the first call, before any sleep.
        assert_eq!(gaps[0], Duration::ZERO);
        assert_eq!(gaps[1], FIRST_DELAY);
        assert_eq!(gaps[2], FIRST_DELAY * 2);
        assert_eq!(gaps[3], FIRST_DELAY * 4);
        assert_eq!(gaps[4], FIRST_DELAY * 8);
        assert_eq!(gaps[5], FIRST_DELAY * 16);
        // 2ms doubled five times is 64ms, past the ceiling, so it clamps and
        // stays there rather than growing without bound.
        assert_eq!(gaps[6], MAX_DELAY);
        assert_eq!(gaps[7], MAX_DELAY);
    }

    /// The window is a platform decision, so assert the decision rather than a
    /// number: Windows has to bind a named pipe and detach into a job object,
    /// and a supervisor starting many sessions serialises those.
    #[test]
    fn windows_gets_a_longer_readiness_window_than_unix() {
        // Arithmetic rather than a branch: a `match`/`if` on `cfg!` leaves the
        // other platform's arm unreachable here, which the 100% gate reads as an
        // uncovered region.
        let expected = 5 + 10 * u64::from(cfg!(windows));
        assert_eq!(READY_TIMEOUT.as_secs(), expected);
        // Whatever the platform, the window has to outlast the backoff ceiling
        // by enough to poll more than once - a window shorter than MAX_DELAY
        // would give up after a single sleep.
        assert!(READY_TIMEOUT > MAX_DELAY * 10);
    }
}
