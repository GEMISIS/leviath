//! In-memory interaction hub - the shared-world replacement for the imperative
//! worker's `pending.json`/`response.json` file polling.
//!
//! When an agent's tool execution needs human input (an `ask_user_*` /
//! `present_for_review` tool, or a tool-approval prompt), its
//! [`HubInteractionBackend::ask`] registers the [`InteractionRequest`] with the
//! [`InteractionHub`] and awaits a oneshot for the answer. The daemon surfaces
//! open requests over the control channel via [`InteractionHub::pending`] and
//! delivers answers with [`InteractionHub::answer`] - no filesystem, no polling.
//!
//! `ask` blocks its caller until the request is answered or cancelled, which for
//! a person at a keyboard can be a very long time. When the caller is a tool
//! batch it waits [`off_lane`](crate::tool_bridge::off_lane), so a prompt nobody
//! has answered yet costs the tool lane no capacity.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use bevy_ecs::prelude::Resource;
use leviath_core::interaction::{InteractionRequest, InteractionResponse};
use tokio::sync::{Notify, oneshot};

use crate::dynamic_interaction::InteractionBackend;

/// One open interaction awaiting an answer.
struct PendingEntry {
    /// The agent (by id) that raised the request.
    agent_id: String,
    /// The request itself (surfaced to clients).
    request: InteractionRequest,
    /// Fulfilled by [`InteractionHub::answer`]; dropped by [`InteractionHub::cancel`].
    responder: oneshot::Sender<InteractionResponse>,
}

/// A process-wide registry of open interactions, keyed by request id. Cheap to
/// clone (shared `Arc`). Also a bevy [`Resource`] so the tick loop's
/// [`reflect_interaction_status`](crate::pipeline::reflect_interaction_status)
/// system can mirror open requests into agent status.
#[derive(Clone, Default, Resource)]
pub struct InteractionHub {
    pending: Arc<Mutex<HashMap<String, PendingEntry>>>,
    /// The tick-loop wake handle, attached once by
    /// [`PipelineWorld::insert_interaction_hub`](crate::world::PipelineWorld::insert_interaction_hub).
    /// Opening, answering, or cancelling a request nudges it so the loop ticks
    /// (while otherwise parked) and reflects the change into agent status.
    wake: Arc<OnceLock<Arc<Notify>>>,
}

impl InteractionHub {
    /// A fresh, empty hub.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the tick-loop wake handle so registry changes wake the driver.
    /// Idempotent: a second call is ignored (the handle is set once at startup).
    pub fn attach_wake(&self, wake: Arc<Notify>) {
        let _ = self.wake.set(wake);
    }

    /// Wake the tick loop if a handle is attached (no-op otherwise).
    fn nudge(&self) {
        if let Some(wake) = self.wake.get() {
            wake.notify_one();
        }
    }

    /// Register a request from `agent_id` and await its answer. Returns a neutral
    /// (empty-text) response if the request is cancelled before it is answered.
    async fn submit(&self, agent_id: &str, request: InteractionRequest) -> InteractionResponse {
        let id = request.id.clone();
        let (responder, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                id.clone(),
                PendingEntry {
                    agent_id: agent_id.to_string(),
                    request,
                    responder,
                },
            );
        // Wake the driver so it ticks and reflects this open request into the
        // agent's status (Active → Waiting) for the dashboard to surface.
        self.nudge();
        // The lock is released before awaiting; answer()/cancel() can run.
        //
        // Off the tool lane, because there is no bound on how long a person
        // takes: a batch that held lane capacity through a prompt was capacity
        // no other agent's tools could use (issue #191). Callers that are not
        // tool batches - the gate-prompt and interaction-point lanes - have no
        // ticket, and for them this is a plain await.
        crate::tool_bridge::off_lane(rx)
            .await
            .unwrap_or_else(|_| InteractionResponse::text(id, ""))
    }

    /// Every open request, as `(agent_id, request)` pairs, for surfacing to
    /// clients.
    pub fn pending(&self) -> Vec<(String, InteractionRequest)> {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|e| (e.agent_id.clone(), e.request.clone()))
            .collect()
    }

    /// Answer an open request. Returns `false` if no request with that id is
    /// open (already answered, cancelled, or never existed).
    pub fn answer(&self, response: InteractionResponse) -> bool {
        let entry = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&response.request_id);
        match entry {
            Some(entry) => {
                // The awaiting `submit` may have gone away (agent despawned); a
                // failed send is harmless.
                let _ = entry.responder.send(response);
                // Wake the driver so it reflects the now-cleared request back
                // into the agent's status (Waiting → Active).
                self.nudge();
                true
            }
            None => false,
        }
    }

    /// Cancel an open request (its `submit` returns the neutral response).
    /// Returns `false` if no such request is open.
    pub fn cancel(&self, request_id: &str) -> bool {
        // Dropping the entry drops its responder, waking `submit` with an error.
        let removed = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(request_id)
            .is_some();
        if removed {
            self.nudge();
        }
        removed
    }

    /// Cancel every open request belonging to `agent_id`, returning how many were
    /// closed. Each one's `submit` wakes with the neutral response.
    ///
    /// This is the per-agent counterpart of [`Self::cancel`], which is keyed by
    /// request id - an id a canceller of a *run* doesn't have. Without it,
    /// cancelling a run left its `ask` future blocked forever, and the orphaned
    /// request kept being surfaced by `lev respond` and the dashboard for a run
    /// that no longer exists.
    pub fn cancel_for_agent(&self, agent_id: &str) -> usize {
        // Dropping each entry drops its responder, waking `submit` with an error.
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let before = pending.len();
        pending.retain(|_, entry| entry.agent_id != agent_id);
        let removed = before - pending.len();
        drop(pending);
        if removed > 0 {
            self.nudge();
        }
        removed
    }

    /// A per-agent [`InteractionBackend`] backed by this hub.
    pub fn backend_for(&self, agent_id: impl Into<String>) -> HubInteractionBackend {
        HubInteractionBackend {
            hub: self.clone(),
            agent_id: agent_id.into(),
        }
    }
}

/// A per-agent [`InteractionBackend`] that routes `ask` through an
/// [`InteractionHub`].
#[derive(Clone)]
pub struct HubInteractionBackend {
    hub: InteractionHub,
    agent_id: String,
}

#[async_trait::async_trait]
impl InteractionBackend for HubInteractionBackend {
    async fn ask(&self, request: InteractionRequest) -> InteractionResponse {
        self.hub.submit(&self.agent_id, request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(id: &str) -> InteractionRequest {
        InteractionRequest::free_text(id, "prompt?", "stage", true)
    }

    /// Let a just-spawned `submit` task reach its await point. `submit` inserts
    /// into the registry synchronously before awaiting, so a few yields on the
    /// current-thread test runtime are enough for it to have registered.
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    #[test]
    fn a_poisoned_registry_still_serves_every_other_agent() {
        // `pending` holds *every* agent's open prompt, so a panic while holding
        // it must not poison it: a poisoned registry makes
        // `pending()`/`answer()`/`cancel()` panic for all agents and the
        // dashboard (issue #109).
        let hub = InteractionHub::new();
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // silence the deliberate panic
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = hub.pending.lock().expect("fresh lock");
            panic!("a panic while holding the interaction registry");
        }));
        std::panic::set_hook(prev);
        assert!(poisoned.is_err());
        assert!(hub.pending.is_poisoned(), "the lock really is poisoned");

        assert!(hub.pending().is_empty());
        assert!(!hub.cancel("nope"));
        assert!(!hub.answer(InteractionResponse::text("nope", "x")));
    }

    #[tokio::test]
    async fn ask_is_answered_through_the_hub() {
        let hub = InteractionHub::new();
        let backend = hub.backend_for("agent-a");
        let asking = tokio::spawn(async move { backend.ask(req("q1")).await });

        settle().await;
        let pending = hub.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, "agent-a");
        assert_eq!(pending[0].1.id, "q1");

        assert!(hub.answer(InteractionResponse::text("q1", "hello")));
        let response = asking.await.unwrap();
        assert_eq!(response.value.as_deref(), Some("hello"));
        // No longer pending.
        assert!(hub.pending().is_empty());
    }

    /// An unanswered prompt must not hold tool-lane capacity.
    ///
    /// The answer can arrive from another agent's tool call, and on a lane with
    /// no room left that call is queued behind the batch that is waiting for it.
    /// That is the shape that froze whole factories in issue #191: everything
    /// looked `waiting`, nothing was failed, and nothing ever moved again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_batch_waiting_on_a_prompt_does_not_hold_the_tool_lane() {
        use crate::tool_bridge::{ToolJob, ToolLane, ToolLaneStats};
        use bevy_ecs::entity::Entity;

        let hub = InteractionHub::new();
        let (job_tx, job_rx) = tokio::sync::mpsc::unbounded_channel();
        let (result_tx, mut results) = tokio::sync::mpsc::unbounded_channel();
        let stats = Arc::new(ToolLaneStats::new(1));
        let lane = ToolLane::new(
            tokio::runtime::Handle::current(),
            result_tx,
            Arc::new(Notify::new()),
            1,
            stats.clone(),
        );
        let serving = lane.serve(job_rx);
        let submit = |entity: u32, exec: crate::tool_bridge::BoxedToolExec| {
            stats.enqueued();
            job_tx
                .send(ToolJob {
                    entity: Entity::from_raw_u32(entity).expect("a small index is a valid id"),
                    exec,
                    cancel: crate::cancel::CancelToken::new(),
                })
                .expect("the lane is serving");
        };

        // The whole lane, spent on waiting for an answer.
        let asking = hub.backend_for("agent-a");
        submit(
            1,
            Box::new(move || {
                Box::pin(async move {
                    let response = asking.ask(req("q1")).await;
                    vec![("q1".to_string(), response.value.unwrap_or_default())]
                })
            }),
        );
        wait_for_prompt(&hub).await;
        assert_eq!(stats.parked(), 1, "the asker stepped off the lane");

        // The answer, as another batch - only reachable if the lane is free.
        let answering = hub.clone();
        submit(
            2,
            Box::new(move || {
                Box::pin(async move {
                    answering.answer(InteractionResponse::text("q1", "hello"));
                    vec![("answered".to_string(), "ok".to_string())]
                })
            }),
        );

        let mut answers = Vec::new();
        for _ in 0..2 {
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), results.recv())
                .await
                .expect("both batches finished")
                .expect("an outcome arrived");
            answers.extend(outcome.results);
        }
        answers.sort();
        assert_eq!(
            answers,
            vec![
                ("answered".to_string(), "ok".to_string()),
                ("q1".to_string(), "hello".to_string()),
            ],
            "the asker got its answer from the batch behind it"
        );

        drop(job_tx);
        tokio::time::timeout(std::time::Duration::from_secs(30), serving)
            .await
            .expect("the lane drained")
            .expect("the lane task ended");
    }

    /// Block until a prompt is registered. `submit` inserts synchronously before
    /// awaiting, but on a multi-threaded runtime the batch task may not have been
    /// polled yet, so yielding a fixed number of times is not enough.
    async fn wait_for_prompt(hub: &InteractionHub) {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while hub.pending().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the prompt was raised");
    }

    #[tokio::test]
    async fn answer_unknown_request_is_false() {
        let hub = InteractionHub::new();
        assert!(!hub.answer(InteractionResponse::text("nope", "x")));
    }

    #[tokio::test]
    async fn submit_and_answer_nudge_the_attached_wake() {
        let hub = InteractionHub::new();
        let wake = Arc::new(Notify::new());
        hub.attach_wake(wake.clone());
        // A second attach is ignored - the handle is set once at startup.
        hub.attach_wake(Arc::new(Notify::new()));

        let backend = hub.backend_for("agent-a");
        let asking = tokio::spawn(async move { backend.ask(req("q1")).await });
        settle().await;

        // submit() nudged the original wake.
        wake.notified().await;

        // answer() nudges it again (consume the submit permit first).
        assert!(hub.answer(InteractionResponse::text("q1", "hi")));
        wake.notified().await;
        assert_eq!(asking.await.unwrap().value.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn cancel_nudges_the_attached_wake() {
        let hub = InteractionHub::new();
        let wake = Arc::new(Notify::new());
        hub.attach_wake(wake.clone());

        let backend = hub.backend_for("agent-a");
        let asking = tokio::spawn(async move { backend.ask(req("q2")).await });
        settle().await;
        wake.notified().await; // drain the submit nudge

        assert!(hub.cancel("q2"));
        wake.notified().await; // cancel nudged the wake
        let _ = asking.await.unwrap();
    }

    #[tokio::test]
    async fn cancel_wakes_submit_with_neutral_response() {
        let hub = InteractionHub::new();
        let backend = hub.backend_for("agent-a");
        let asking = tokio::spawn(async move { backend.ask(req("q2")).await });

        settle().await;
        assert!(hub.cancel("q2"));
        let response = asking.await.unwrap();
        assert_eq!(response.request_id, "q2");
        assert_eq!(response.value.as_deref(), Some("")); // neutral

        // Cancelling again ⇒ nothing to cancel.
        assert!(!hub.cancel("q2"));
    }
}
