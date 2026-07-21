//! In-memory interaction hub — the shared-world replacement for the imperative
//! worker's `pending.json`/`response.json` file polling.
//!
//! When an agent's tool execution needs human input (an `ask_user_*` /
//! `present_for_review` tool, or a tool-approval prompt), its
//! [`HubInteractionBackend::ask`] registers the [`InteractionRequest`] with the
//! [`InteractionHub`] and awaits a oneshot for the answer. The daemon surfaces
//! open requests over the control channel via [`InteractionHub::pending`] and
//! delivers answers with [`InteractionHub::answer`] — no filesystem, no polling.
//!
//! `ask` blocks the calling tool worker until the request is answered or
//! cancelled; with the tool lane sequential today that serializes interactive
//! agents, which is acceptable (the lane becomes a pool later).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

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
        self.pending.lock().unwrap().insert(
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
        rx.await
            .unwrap_or_else(|_| InteractionResponse::text(id, ""))
    }

    /// Every open request, as `(agent_id, request)` pairs, for surfacing to
    /// clients.
    pub fn pending(&self) -> Vec<(String, InteractionRequest)> {
        self.pending
            .lock()
            .unwrap()
            .values()
            .map(|e| (e.agent_id.clone(), e.request.clone()))
            .collect()
    }

    /// Answer an open request. Returns `false` if no request with that id is
    /// open (already answered, cancelled, or never existed).
    pub fn answer(&self, response: InteractionResponse) -> bool {
        let entry = self.pending.lock().unwrap().remove(&response.request_id);
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
        let removed = self.pending.lock().unwrap().remove(request_id).is_some();
        if removed {
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
        // A second attach is ignored — the handle is set once at startup.
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
