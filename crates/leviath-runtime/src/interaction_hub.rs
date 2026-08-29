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
//!
//! "A very long time" used to mean "for ever": a run whose operator had walked
//! away sat in `WaitingInput` until the daemon died, holding its slot the whole
//! time (issue #204). [`InteractionHub::set_timeout_secs`] puts a deadline on
//! that wait.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

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
/// `reflect_interaction_status`
/// system can mirror open requests into agent status.
#[derive(Clone, Default, Resource)]
pub struct InteractionHub {
    pending: Arc<Mutex<HashMap<String, PendingEntry>>>,
    /// The tick-loop wake handle, attached once by
    /// [`PipelineWorld::insert_interaction_hub`](crate::world::PipelineWorld::insert_interaction_hub).
    /// Opening, answering, or cancelling a request nudges it so the loop ticks
    /// (while otherwise parked) and reflects the change into agent status.
    wake: Arc<OnceLock<Arc<Notify>>>,
    /// How long an open request may go unanswered before the hub resolves it
    /// itself, in seconds. `0` (the default) waits indefinitely. Set once at
    /// daemon start from `[limits] interaction_timeout_secs`.
    timeout_secs: Arc<AtomicU64>,
}

/// The default deadline on an unanswered prompt, in seconds.
///
/// An hour is long enough that a person who is actually there answers well
/// inside it, and short enough that a run whose operator has gone home releases
/// its slot the same day rather than holding it until the daemon restarts.
pub const DEFAULT_INTERACTION_TIMEOUT_SECS: u64 = 3600;

impl InteractionHub {
    /// A fresh, empty hub.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the tick-loop wake handle so registry changes wake the driver.
    /// Idempotent: a second call is ignored (the handle is set once at startup).
    pub(crate) fn attach_wake(&self, wake: Arc<Notify>) {
        let _ = self.wake.set(wake);
    }

    /// Set how long an open request may go unanswered before the hub resolves it
    /// itself. `0` waits indefinitely - the behaviour before issue #204.
    ///
    /// Applies to requests opened from here on; a request already parked keeps
    /// the deadline it was opened with.
    pub fn set_timeout_secs(&self, secs: u64) {
        self.timeout_secs.store(secs, Ordering::Relaxed);
    }

    /// The current deadline, or `None` when the hub waits indefinitely.
    fn timeout(&self) -> Option<Duration> {
        match self.timeout_secs.load(Ordering::Relaxed) {
            0 => None,
            secs => Some(Duration::from_secs(secs)),
        }
    }

    /// Wake the tick loop if a handle is attached (no-op otherwise).
    fn nudge(&self) {
        if let Some(wake) = self.wake.get() {
            wake.notify_one();
        }
    }

    /// Register a request from `agent_id` and await its answer. Returns a neutral
    /// (empty-text) response if the request is cancelled before it is answered,
    /// or if it goes unanswered past [`set_timeout_secs`](Self::set_timeout_secs).
    ///
    /// The timeout deliberately produces the *same* neutral response a cancel
    /// does, so nothing downstream has to learn a third outcome: an approval or
    /// a taint gate reads it as not-approved and denies, an `ask_user_*` tool
    /// reports that no answer came, and an interaction point proceeds with empty
    /// user text - each exactly as it already behaves for a cancelled request.
    async fn submit(&self, agent_id: &str, request: InteractionRequest) -> InteractionResponse {
        let id = request.id.clone();
        let (responder, rx) = oneshot::channel();
        leviath_core::sync::lock(&self.pending).insert(
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
        let Some(deadline) = self.timeout() else {
            return crate::tool_bridge::off_lane(rx)
                .await
                .unwrap_or_else(|_| InteractionResponse::text(id, ""));
        };
        // `&mut rx` rather than `rx`, so the receiver outlives an elapsed
        // deadline and a reply that landed in that same instant can still be
        // collected instead of thrown away.
        let mut rx = rx;
        match crate::tool_bridge::off_lane(tokio::time::timeout(deadline, &mut rx)).await {
            Ok(answered) => answered.unwrap_or_else(|_| InteractionResponse::text(id, "")),
            Err(_elapsed) => self.expire(agent_id, &id, &mut rx),
        }
    }

    /// Resolve a request nobody answered in time: drop it from the open set so
    /// the tick loop takes the agent out of `Waiting`, and hand its caller the
    /// neutral response.
    ///
    /// A real answer that arrived as the deadline passed still wins. It is
    /// already sitting in the channel, and handing back the neutral response
    /// instead would throw away what a person actually said.
    fn expire(
        &self,
        agent_id: &str,
        id: &str,
        rx: &mut oneshot::Receiver<InteractionResponse>,
    ) -> InteractionResponse {
        leviath_core::sync::lock(&self.pending).remove(id);
        if let Ok(answered) = rx.try_recv() {
            return answered;
        }
        tracing::warn!(
            agent = %agent_id,
            request = %id,
            "no answer within the interaction timeout - resolving it as unanswered"
        );
        // Wake the driver so `reflect_interaction_status` moves the agent from
        // Waiting back to Active now, rather than at the next re-drive.
        self.nudge();
        InteractionResponse::text(id, "")
    }

    /// Every open request, as `(agent_id, request)` pairs, for surfacing to
    /// clients.
    pub fn pending(&self) -> Vec<(String, InteractionRequest)> {
        leviath_core::sync::lock(&self.pending)
            .values()
            .map(|e| (e.agent_id.clone(), e.request.clone()))
            .collect()
    }

    /// Answer an open request. Returns `false` if no request with that id is
    /// open (already answered, cancelled, or never existed).
    pub fn answer(&self, response: InteractionResponse) -> bool {
        let entry = leviath_core::sync::lock(&self.pending).remove(&response.request_id);
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
    pub(crate) fn cancel(&self, request_id: &str) -> bool {
        // Dropping the entry drops its responder, waking `submit` with an error.
        let removed = leviath_core::sync::lock(&self.pending)
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
    pub(crate) fn cancel_for_agent(&self, agent_id: &str) -> usize {
        // Dropping each entry drops its responder, waking `submit` with an error.
        let mut pending = leviath_core::sync::lock(&self.pending);
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
#[cfg(test)]
#[path = "interaction_hub_tests.rs"]
mod tests;

/// Where a prompt's answer goes once a person gives one.
///
/// Both prompt paths - the taint gate and blueprint interaction points - are the
/// same three things: the hub that owns the conversation, the channel the
/// resolution is reported on, and the driver to wake once it is. Only the
/// outcome type differs, so this is generic over it rather than written twice.
pub(crate) struct PromptLane<T> {
    /// The hub that owns the conversation with the user.
    pub hub: InteractionHub,
    /// The channel the resolution is reported on.
    pub outcomes: tokio::sync::mpsc::UnboundedSender<T>,
    /// The driver to wake once it is.
    pub wake: std::sync::Arc<tokio::sync::Notify>,
}
