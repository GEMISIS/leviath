//! Supervision for the async lanes, so a task that dies still reports.
//!
//! Every lane between the ECS and the network follows the same contract: a
//! dispatch system marks the agent `Awaiting…`, `tokio::spawn`s a job, and the
//! matching collect system clears the marker when the job's outcome lands. The
//! agent's whole forward progress therefore rests on the outcome arriving.
//!
//! A dropped [`JoinHandle`](tokio::task::JoinHandle) breaks that contract
//! silently. If the job panics before its send, no outcome is ever produced -
//! and the marker it is waiting on is one of the `has_async_inflight` states, so
//! the driver treats the agent as busy rather than quiescent and it waits for a
//! completion that can no longer happen (issue #190). The panic guard around the
//! tick schedule (`world::run_isolated`) does not help here: these panics happen
//! on a tokio worker, not in a system.
//!
//! [`spawn_supervised`] keeps the handle and turns a dead task back into an
//! ordinary lane error, which every collect system already knows how to apply.

use std::future::Future;

use tokio::runtime::Handle;
use tokio::task::JoinError;

/// Spawn `job` on `runtime` and watch it: if it ends without reporting - it
/// panicked, or was aborted - hand `report_lost` a message describing what
/// happened so the caller can synthesize the outcome its lane owes the agent.
///
/// `report_lost` runs only on that failure path. A job that returns normally
/// (including a cancelled inference, which deliberately reports nothing) is left
/// entirely alone.
pub(crate) fn spawn_supervised<F>(
    runtime: &Handle,
    lane: &'static str,
    job: F,
    report_lost: impl FnOnce(String) + Send + 'static,
) where
    F: Future<Output = ()> + Send + 'static,
{
    let inner = runtime.clone();
    runtime.spawn(async move {
        let Err(e) = inner.spawn(job).await else {
            return; // reported for itself
        };
        let message = lost_lane_message(lane, e);
        tracing::error!(lane, %message, "an async lane task died without reporting");
        report_lost(message);
    });
}

/// Describe a lane task that ended without reporting, for the error outcome the
/// supervisor synthesizes in its place.
///
/// Split out (rather than inlined above) so both endings a [`JoinError`] can
/// carry are exercised directly, without having to drive a real lane into each.
pub(crate) fn lost_lane_message(lane: &str, err: JoinError) -> String {
    if !err.is_panic() {
        return format!("the {lane} task was cancelled before it reported a result");
    }
    let payload = err.into_panic();
    format!(
        "the {lane} task panicked and reported nothing: {}",
        leviath_core::panic_message(payload.as_ref())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    use crate::test_support::SilentPanics;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_job_is_reported_instead_of_vanishing() {
        let _silent = SilentPanics::install();
        let (tx, mut rx) = mpsc::unbounded_channel();
        spawn_supervised(
            &Handle::current(),
            "inference",
            async {
                panic!("the provider adapter blew up");
            },
            move |message| {
                let _ = tx.send(message);
            },
        );

        let message = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("the supervisor reports promptly")
            .expect("a message");
        assert!(message.contains("inference"), "got: {message}");
        assert!(
            message.contains("the provider adapter blew up"),
            "the panic text must survive so the run says why it failed, got: {message}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_job_that_reports_for_itself_is_left_alone() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        spawn_supervised(&Handle::current(), "inference", async {}, move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        });
        // Give the supervisor every chance to fire spuriously.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn lost_lane_message_names_the_lane_and_the_ending() {
        // A panicking task's JoinError carries the payload.
        let silent = SilentPanics::install();
        let err = tokio::spawn(async { panic!("boom") })
            .await
            .expect_err("the task panicked");
        drop(silent);
        let message = lost_lane_message("compaction", err);
        assert!(message.contains("compaction"), "got: {message}");
        assert!(message.contains("boom"), "got: {message}");

        // An aborted task's does not - it never ran to a panic.
        let handle = tokio::spawn(async {
            std::future::pending::<()>().await;
        });
        handle.abort();
        let err = handle.await.expect_err("the task was aborted");
        let message = lost_lane_message("transition", err);
        assert!(message.contains("transition"), "got: {message}");
        assert!(message.contains("cancelled"), "got: {message}");
    }
}
