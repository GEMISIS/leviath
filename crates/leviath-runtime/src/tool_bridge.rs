//! The async worker side of the ECS tool stage — the sync-ECS ↔ async-I/O
//! bridge for tool execution.
//!
//! When the pipeline decides an agent's response has tool calls to run, the
//! tool-dispatch system builds a [`ToolJob`] (the agent plus a boxed async
//! closure that executes that agent's batch of calls against its own tool
//! registry / workdir / policy) and sends it to the tool lane. Each
//! [`tool_worker`] pulls jobs and reports each [`ToolOutcome`] back on the
//! results channel, waking the tick loop; the tool-collect system applies the
//! results on a later tick.
//!
//! **Concurrency**: the lane is a *pool* — [`spawn_tool_pool`] runs N workers off
//! one shared job channel (`Arc<Mutex<Receiver>>`). A worker holds the receiver
//! lock only across `recv()`, then releases it and runs `exec().await`, so up to
//! N agents' tool batches execute concurrently (the receive is serialized; the
//! execution is not). This is the tool-lane counterpart of the inference pool's
//! per-model concurrency cap. The dispatch/collect systems are unchanged.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bevy_ecs::entity::Entity;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

/// The future produced by a boxed tool-execution closure: resolves to
/// `(tool_call_id, result)` pairs — the same shape the engine's tool executors
/// already return.
pub type ToolExecFuture = Pin<Box<dyn Future<Output = Vec<(String, String)>> + Send>>;

/// A boxed, per-agent tool-execution closure. Built by the dispatch system so it
/// captures that agent's own tool registry, workdir, and policy; run once by the
/// tool worker.
pub type BoxedToolExec = Box<dyn FnOnce() -> ToolExecFuture + Send>;

/// A batch of tool calls to execute for one agent.
pub struct ToolJob {
    /// The agent the calls belong to.
    pub entity: Entity,
    /// Runs the agent's batch of tool calls.
    pub exec: BoxedToolExec,
    /// Fires when the agent is cancelled, so the worker drops the batch instead
    /// of running it to completion. The agent holds the other half.
    pub cancel: crate::cancel::CancelToken,
}

/// The result of a [`ToolJob`], applied on a later tick by the tool-collect
/// system.
pub struct ToolOutcome {
    /// The agent the results belong to.
    pub entity: Entity,
    /// `(tool_call_id, result)` pairs.
    pub results: Vec<(String, String)>,
}

/// A shared, multi-consumer job receiver: several [`tool_worker`]s pull from the
/// same channel by taking the lock only long enough to `recv()`.
pub type SharedJobRx = Arc<Mutex<UnboundedReceiver<ToolJob>>>;

/// One tool worker: pulls [`ToolJob`]s from the shared channel and runs them,
/// reporting each outcome and waking the tick loop. Holds the receiver lock only
/// across `recv()` (so sibling workers can run their `exec().await` in parallel),
/// then releases it before executing. Returns when the job channel is closed (all
/// senders dropped — i.e. the world is shutting down).
pub async fn tool_worker(
    jobs: SharedJobRx,
    results: UnboundedSender<ToolOutcome>,
    wake: Arc<Notify>,
) {
    loop {
        // Serialize only the receive; drop the guard before executing.
        let next = {
            let mut rx = jobs.lock().await;
            rx.recv().await
        };
        let Some(ToolJob {
            entity,
            exec,
            cancel,
        }) = next
        else {
            return; // channel closed → shut down
        };
        // A cancelled agent's batch is dropped rather than run to completion.
        // This is what returns the worker to the pool: several of the things a
        // batch can await are unbounded (a tool-approval prompt, an `ask_user`,
        // a `wait_for_agent` poll), so without this a cancelled agent kept one
        // of the lane's fixed number of workers forever — and once they were all
        // taken, no other agent's tools ran either.
        let out = tokio::select! {
            biased;
            _ = cancel.cancelled() => continue,
            out = exec() => out,
        };
        // Harmless no-op if the collect side has gone away.
        let _ = results.send(ToolOutcome {
            entity,
            results: out,
        });
        wake.notify_one();
    }
}

/// Spawn a pool of `workers` [`tool_worker`] tasks off one shared job channel and
/// return their handles. `workers` is clamped to at least 1. This is the tool-lane
/// concurrency cap — the number of agents whose tool batches may run at once.
pub fn spawn_tool_pool(
    runtime: &Handle,
    jobs: UnboundedReceiver<ToolJob>,
    results: UnboundedSender<ToolOutcome>,
    wake: Arc<Notify>,
    workers: usize,
) -> Vec<JoinHandle<()>> {
    let shared: SharedJobRx = Arc::new(Mutex::new(jobs));
    (0..workers.max(1))
        .map(|_| runtime.spawn(tool_worker(shared.clone(), results.clone(), wake.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn job(entity: u32, pairs: Vec<(&'static str, &'static str)>) -> ToolJob {
        job_with(entity, pairs, crate::cancel::CancelToken::new())
    }

    fn job_with(
        entity: u32,
        pairs: Vec<(&'static str, &'static str)>,
        cancel: crate::cancel::CancelToken,
    ) -> ToolJob {
        ToolJob {
            entity: Entity::from_raw(entity),
            exec: Box::new(move || {
                Box::pin(async move {
                    pairs
                        .into_iter()
                        .map(|(a, b)| (a.to_string(), b.to_string()))
                        .collect()
                })
            }),
            cancel,
        }
    }

    /// A job whose batch blocks until `release` fires, signalling `started` once
    /// it is running. Used both for the cancel case (where it is dropped
    /// mid-flight) and for the released case, so the batch body is exercised.
    fn held_job(
        entity: u32,
        started: Arc<Notify>,
        release: Arc<Notify>,
        cancel: crate::cancel::CancelToken,
    ) -> ToolJob {
        ToolJob {
            entity: Entity::from_raw(entity),
            exec: Box::new(move || {
                Box::pin(async move {
                    // `notify_one` (not `notify_waiters`) on both signals: it
                    // stores a permit when nobody is waiting yet, so neither the
                    // test nor the batch can lose the other's wakeup by being
                    // slow to arm — a real flake on a loaded runner.
                    started.notify_one();
                    release.notified().await;
                    vec![("held".to_string(), "done".to_string())]
                })
            }),
            cancel,
        }
    }

    /// The released counterpart of [`held_job`]: with no cancel, the batch runs
    /// to completion and reports its results.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_released_batch_completes_normally() {
        let (jtx, jrx) = mpsc::unbounded_channel();
        let (rtx, mut rrx) = mpsc::unbounded_channel();
        let wake = Arc::new(Notify::new());
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());

        jtx.send(held_job(
            7,
            started.clone(),
            release.clone(),
            crate::cancel::CancelToken::new(),
        ))
        .unwrap();
        drop(jtx);

        let handles = spawn_tool_pool(&Handle::current(), jrx, rtx, wake, 1);
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .expect("the batch started");
        release.notify_one();

        let out = tokio::time::timeout(std::time::Duration::from_secs(5), rrx.recv())
            .await
            .expect("the batch finished")
            .expect("an outcome arrived");
        assert_eq!(out.results, vec![("held".to_string(), "done".to_string())]);
        for h in handles {
            let _ = h.await;
        }
    }

    fn shared(rx: UnboundedReceiver<ToolJob>) -> SharedJobRx {
        Arc::new(Mutex::new(rx))
    }

    #[tokio::test]
    async fn worker_processes_jobs_in_order_then_exits_on_close() {
        let (jtx, jrx) = mpsc::unbounded_channel();
        let (rtx, mut rrx) = mpsc::unbounded_channel();
        let wake = Arc::new(Notify::new());

        jtx.send(job(1, vec![("c1", "r1")])).unwrap();
        jtx.send(job(2, vec![("c2", "r2")])).unwrap();
        drop(jtx); // close the job channel so the worker loop ends

        tool_worker(shared(jrx), rtx, wake).await;

        let first = rrx.try_recv().unwrap();
        assert_eq!(first.entity, Entity::from_raw(1));
        assert_eq!(first.results, vec![("c1".to_string(), "r1".to_string())]);
        let second = rrx.try_recv().unwrap();
        assert_eq!(second.entity, Entity::from_raw(2));
        assert!(rrx.try_recv().is_err()); // no more outcomes
    }

    /// A cancelled batch is dropped, not run to completion, and the worker goes
    /// back to serving the lane.
    ///
    /// This is what unwedges the daemon: several things a batch can await are
    /// unbounded (a tool-approval prompt, `ask_user`, a `wait_for_agent` poll),
    /// and the lane has a fixed number of workers — so a cancelled agent used to
    /// hold one forever, and once all of them were held no agent's tools ran.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_batch_is_abandoned_and_frees_the_worker() {
        let (jtx, jrx) = mpsc::unbounded_channel();
        let (rtx, mut rrx) = mpsc::unbounded_channel();
        let wake = Arc::new(Notify::new());
        let cancel = crate::cancel::CancelToken::new();

        // A batch that only finishes when released — the unbounded-prompt case.
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        jtx.send(held_job(
            1,
            started.clone(),
            release.clone(),
            cancel.clone(),
        ))
        .unwrap();
        // Queued behind it: only reachable once the worker is released.
        jtx.send(job(2, vec![("c2", "r2")])).unwrap();
        drop(jtx);

        let handles = spawn_tool_pool(&Handle::current(), jrx, rtx, wake, 1);
        // Wait until the stuck batch is actually executing, then cancel it.
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .expect("the batch started");
        cancel.cancel();

        // The single worker moves on to the next job rather than staying parked.
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), rrx.recv())
            .await
            .expect("the worker was freed by the cancel")
            .expect("an outcome arrived");
        assert_eq!(next.entity, Entity::from_raw(2), "the queued batch ran");
        assert!(
            rrx.try_recv().is_err(),
            "and the cancelled batch reported no results"
        );
        for h in handles {
            let _ = h.await;
        }
    }

    #[tokio::test]
    async fn worker_survives_dropped_results_receiver() {
        let (jtx, jrx) = mpsc::unbounded_channel();
        let (rtx, rrx) = mpsc::unbounded_channel();
        drop(rrx); // nobody to receive outcomes
        let wake = Arc::new(Notify::new());

        jtx.send(job(9, vec![("c", "r")])).unwrap();
        drop(jtx);
        // Must drain the job and not panic despite the failed send.
        tool_worker(shared(jrx), rtx, wake).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn pool_runs_batches_concurrently_and_all_exit_on_close() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let (jtx, jrx) = mpsc::unbounded_channel();
        let (rtx, mut rrx) = mpsc::unbounded_channel();
        let wake = Arc::new(Notify::new());

        // Three jobs that each block on a barrier: they can only all finish if
        // they run concurrently (a single-worker lane would deadlock the test's
        // timeout). A shared counter + Notify serves as the rendezvous.
        let arrived = Arc::new(AtomicUsize::new(0));
        let go = Arc::new(Notify::new());
        for i in 1..=3u32 {
            let arrived = arrived.clone();
            let go = go.clone();
            jtx.send(ToolJob {
                entity: Entity::from_raw(i),
                exec: Box::new(move || {
                    Box::pin(async move {
                        if arrived.fetch_add(1, Ordering::SeqCst) + 1 == 3 {
                            go.notify_waiters();
                        }
                        // Wait until all three have arrived (proving concurrency).
                        while arrived.load(Ordering::SeqCst) < 3 {
                            go.notified().await;
                        }
                        vec![("c".to_string(), "r".to_string())]
                    })
                }),
                cancel: crate::cancel::CancelToken::new(),
            })
            .unwrap();
        }
        drop(jtx);

        let handles = spawn_tool_pool(&Handle::current(), jrx, rtx, wake, 3);
        // All three outcomes must arrive; bounded so a serialization bug fails fast.
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(5), rrx.recv())
                .await
                .expect("all batches complete concurrently")
                .expect("outcome present");
        }
        for h in handles {
            h.await.unwrap(); // every worker exits once the channel closes
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_tool_pool_clamps_zero_to_one_worker() {
        let (jtx, jrx) = mpsc::unbounded_channel();
        let (rtx, mut rrx) = mpsc::unbounded_channel();
        let wake = Arc::new(Notify::new());
        jtx.send(job(7, vec![("c", "r")])).unwrap();
        drop(jtx);
        let handles = spawn_tool_pool(&Handle::current(), jrx, rtx, wake, 0);
        assert_eq!(handles.len(), 1); // clamped up to one worker
        let out = rrx.recv().await.unwrap();
        assert_eq!(out.entity, Entity::from_raw(7));
        for h in handles {
            h.await.unwrap();
        }
    }
}
