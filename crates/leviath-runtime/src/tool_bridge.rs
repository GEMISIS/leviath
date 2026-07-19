//! The async worker side of the ECS tool stage — the sync-ECS ↔ async-I/O
//! bridge for tool execution.
//!
//! When the pipeline decides an agent's response has tool calls to run, the
//! tool-dispatch system builds a [`ToolJob`] (the agent plus a boxed async
//! closure that executes that agent's batch of calls against its own tool
//! registry / workdir / policy) and sends it to the tool lane. [`tool_worker`]
//! processes jobs **one at a time** — tools are sequential for now — and reports
//! each [`ToolOutcome`] back on the results channel, waking the tick loop; the
//! tool-collect system applies the results on a later tick.
//!
//! "Sequential for now" = a single `tool_worker`. It becomes a pool later by
//! running several workers off the same job channel (e.g. a global tool-
//! concurrency cap), with no change to the dispatch/collect systems.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bevy_ecs::entity::Entity;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

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
}

/// The result of a [`ToolJob`], applied on a later tick by the tool-collect
/// system.
pub struct ToolOutcome {
    /// The agent the results belong to.
    pub entity: Entity,
    /// `(tool_call_id, result)` pairs.
    pub results: Vec<(String, String)>,
}

/// The single-lane tool worker: pulls [`ToolJob`]s and runs them **one at a
/// time**, reporting each outcome and waking the tick loop. Returns when the job
/// channel is closed (all senders dropped — i.e. the world is shutting down).
pub async fn tool_worker(
    mut jobs: UnboundedReceiver<ToolJob>,
    results: UnboundedSender<ToolOutcome>,
    wake: Arc<Notify>,
) {
    while let Some(ToolJob { entity, exec }) = jobs.recv().await {
        let out = exec().await;
        // Harmless no-op if the collect side has gone away.
        let _ = results.send(ToolOutcome {
            entity,
            results: out,
        });
        wake.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn job(entity: u32, pairs: Vec<(&'static str, &'static str)>) -> ToolJob {
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
        }
    }

    #[tokio::test]
    async fn worker_processes_jobs_in_order_then_exits_on_close() {
        let (jtx, jrx) = mpsc::unbounded_channel();
        let (rtx, mut rrx) = mpsc::unbounded_channel();
        let wake = Arc::new(Notify::new());

        jtx.send(job(1, vec![("c1", "r1")])).unwrap();
        jtx.send(job(2, vec![("c2", "r2")])).unwrap();
        drop(jtx); // close the job channel so the worker loop ends

        tool_worker(jrx, rtx, wake).await;

        let first = rrx.try_recv().unwrap();
        assert_eq!(first.entity, Entity::from_raw(1));
        assert_eq!(first.results, vec![("c1".to_string(), "r1".to_string())]);
        let second = rrx.try_recv().unwrap();
        assert_eq!(second.entity, Entity::from_raw(2));
        assert!(rrx.try_recv().is_err()); // no more outcomes
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
        tool_worker(jrx, rtx, wake).await;
    }
}
