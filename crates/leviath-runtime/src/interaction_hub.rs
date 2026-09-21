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
//! "A very long time" is, by default, exactly that: a prompt waits until a
//! person answers it or the run is cancelled, however long that takes. An
//! operator who wants an unattended run to release its slot instead sets
//! `[limits] interaction_timeout_secs`, and [`InteractionHub::set_timeout_secs`]
//! puts that deadline on the wait.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use bevy_ecs::prelude::Resource;
use leviath_core::interaction::{InteractionRequest, InteractionResponse, Settlement};
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
    /// Unix seconds when it was asked, so the record can say how long somebody
    /// was kept waiting - or how long the run was.
    asked_at: i64,
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
    /// itself, in seconds. `0` (the default) waits indefinitely: it is how
    /// [`set_timeout_secs`](Self::set_timeout_secs) stores `None`. Written
    /// from `[limits] interaction_timeout_secs` on every config reload, and
    /// read when a request opens, so one already waiting keeps the deadline it
    /// opened with.
    timeout_secs: Arc<AtomicU64>,
    /// Interactions that have settled and are not in the journal yet, each with
    /// the run that asked.
    ///
    /// A buffer rather than a journal handle, because the hub is answered from
    /// outside the tick - over the control socket, from `lev respond` - and the
    /// persistence lane is reached from inside one. `journal_interactions`
    /// drains this every tick.
    settled: Arc<Mutex<Vec<(String, leviath_core::run_archive::InteractionRecord)>>>,
}

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
    /// itself. `None` waits indefinitely, which is also the default; `Some(0)`
    /// is read the same way, so a config that spells "no timeout" as `0` is
    /// not turned into "expire at once".
    ///
    /// Applies to requests opened from here on; a request already parked keeps
    /// the deadline it was opened with.
    pub fn set_timeout_secs(&self, secs: Option<u64>) {
        self.timeout_secs
            .store(secs.unwrap_or(0), Ordering::Relaxed);
    }

    /// The configured deadline in seconds; `None` means the hub waits
    /// indefinitely.
    pub fn timeout_secs(&self) -> Option<u64> {
        match self.timeout_secs.load(Ordering::Relaxed) {
            0 => None,
            secs => Some(secs),
        }
    }

    /// The current deadline, or `None` when the hub waits indefinitely.
    fn timeout(&self) -> Option<Duration> {
        self.timeout_secs().map(Duration::from_secs)
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
        let asked_at = leviath_core::duration::now_secs();
        {
            // One lock for the look and the insert, so two requests arriving
            // together cannot both find the id free.
            let mut pending = leviath_core::sync::lock(&self.pending);
            // Every id carries the run that raised it, so two requests can only
            // meet here if something minted one wrong. The one already open
            // wins: a person may be reading it, and the answer they give names
            // this id and nothing else, so replacing it would hand their answer
            // to whatever arrived last.
            if let Some(open) = pending.get(&id) {
                let already_open_for = open.agent_id.clone();
                tracing::error!(
                    request = %id,
                    already_open_for = %already_open_for,
                    arriving_from = %agent_id,
                    "an interaction request id is already open; refusing the arriving request \
                     rather than replacing the one open"
                );
                // Takes the `settled` lock while holding this one. Nothing goes
                // the other way round: `take_settled` reaches for `settled`
                // alone.
                self.record_one(agent_id, &request, asked_at, Settlement::Refused);
                return InteractionResponse::text(id, "");
            }
            pending.insert(
                id.clone(),
                PendingEntry {
                    agent_id: agent_id.to_string(),
                    request,
                    responder,
                    asked_at,
                },
            );
        }
        // Wake the driver so it ticks and reflects this open request into the
        // agent's status (Active → Waiting) for the dashboard to surface.
        self.nudge();
        // The lock is released before awaiting; answer()/cancel() can run.
        //
        // Off the tool lane, because there is no bound on how long a person
        // takes: a batch that holds lane capacity through a prompt is capacity
        // no other agent's tools can use. Callers that are not
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
        let entry = leviath_core::sync::lock(&self.pending).remove(id);
        // A person did answer, a moment late. Nothing is recorded here:
        // `answer_for` took the entry out and recorded the answer before
        // sending it, so this would be a second record of one decision.
        if let Ok(answered) = rx.try_recv() {
            return answered;
        }
        // Nothing takes a pending entry without recording how it settled, so an
        // entry already gone is one somebody else accounted for - an answer that
        // landed in this same instant, or a cancel.
        entry.inspect(|entry| {
            self.record(entry, Settlement::TimedOut);
        });
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
        self.answer_for(response).is_some()
    }

    /// [`answer`](Self::answer), reporting *whose* request it was.
    ///
    /// The host needs the agent id because answering a prompt is one of the
    /// points a run resumes at, and what a resume does is per-agent.
    pub(crate) fn answer_for(&self, response: InteractionResponse) -> Option<String> {
        let entry = leviath_core::sync::lock(&self.pending).remove(&response.request_id);
        let entry = entry?;
        let agent_id = entry.agent_id.clone();
        self.record(&entry, Settlement::of(&response));
        // The awaiting `submit` may have gone away (agent despawned); a
        // failed send is harmless.
        let _ = entry.responder.send(response);
        // Wake the driver so it reflects the now-cleared request back
        // into the agent's status (Waiting → Active).
        self.nudge();
        Some(agent_id)
    }

    /// Cancel an open request (its `submit` returns the neutral response).
    /// Returns `false` if no such request is open.
    pub(crate) fn cancel(&self, request_id: &str) -> bool {
        // Dropping the entry drops its responder, waking `submit` with an error.
        let entry = leviath_core::sync::lock(&self.pending).remove(request_id);
        let Some(entry) = entry else {
            return false;
        };
        self.record(&entry, Settlement::Cancelled);
        self.nudge();
        true
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
        let mine: Vec<PendingEntry> = pending
            .keys()
            .filter(|id| pending[*id].agent_id == agent_id)
            .cloned()
            .collect::<Vec<String>>()
            .into_iter()
            .filter_map(|id| pending.remove(&id))
            .collect();
        drop(pending);
        let removed = mine.len();
        for entry in &mine {
            self.record(entry, Settlement::Cancelled);
        }
        if removed > 0 {
            self.nudge();
        }
        removed
    }

    /// Put one settled interaction where the journal will find it.
    ///
    /// Called on each way a request is *settled*, because the ways are not
    /// interchangeable to a reader: an answer, a request nobody answered in
    /// time, and one withdrawn when the run was cancelled all hand the caller
    /// the same neutral response, and only this record tells them apart.
    ///
    /// A request refused before it opened is recorded too, under
    /// [`Settlement::Refused`]: its caller was handed the neutral response, and
    /// a run that reads as having denied a tool call needs the journal to say
    /// that nobody denied anything.
    fn record(&self, entry: &PendingEntry, settlement: Settlement) {
        self.record_one(&entry.agent_id, &entry.request, entry.asked_at, settlement);
    }

    /// [`record`](Self::record) for a request with no entry behind it.
    fn record_one(
        &self,
        agent_id: &str,
        request: &InteractionRequest,
        asked_at: i64,
        settlement: Settlement,
    ) {
        leviath_core::sync::lock(&self.settled).push((
            agent_id.to_string(),
            leviath_core::run_archive::InteractionRecord {
                request_id: request.id.clone(),
                kind: request.kind.clone(),
                tool: request.tool_name.clone(),
                prompt: request.prompt.clone(),
                stage: request.stage_name.clone(),
                settlement,
                asked_at,
                at: leviath_core::duration::now_secs(),
            },
        ));
    }

    /// Every settled interaction since the last drain, and the run each belongs
    /// to. What `journal_interactions` sends to the lane.
    pub(crate) fn take_settled(
        &self,
    ) -> Vec<(String, leviath_core::run_archive::InteractionRecord)> {
        std::mem::take(&mut *leviath_core::sync::lock(&self.settled))
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

impl HubInteractionBackend {
    /// The hub's prompt deadline in seconds (`None`: it waits indefinitely), so
    /// a caller can say in its own words how long an unanswered prompt waited.
    pub fn timeout_secs(&self) -> Option<u64> {
        self.hub.timeout_secs()
    }

    /// The run this backend asks on behalf of.
    ///
    /// What a caller minting a request id needs: the id has to carry the run,
    /// because the hub behind this backend is shared with every other run in
    /// the daemon. See
    /// [`request_id`](leviath_core::interaction::request_id).
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }
}

#[async_trait::async_trait]
impl InteractionBackend for HubInteractionBackend {
    async fn ask(&self, request: InteractionRequest) -> InteractionResponse {
        self.hub.submit(&self.agent_id, request).await
    }
}

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
