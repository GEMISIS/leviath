//! What a call in flight has learned about itself, kept where the caller can
//! still read it when the call never returns.
//!
//! A call that fails hands back an error, and one its caller cuts off (a
//! deadline, a cancel) hands back nothing at all. Either may have been billed.
//! What the caller needs to account for it arrives before the ending does: the
//! provider's own id for the call, in the first header or event, and whatever
//! usage the reply had got to. So the caller runs each call inside
//! [`CallTrace::scope`], the provider notes those two things as they arrive
//! ([`note_call_id`], [`note_usage`], [`note_whole_usage`]), and the trace
//! outlives the call.
//!
//! Outside a scope (a provider called directly, a test) there is no trace and
//! the notes go nowhere.

use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};

use crate::pricing::TokenUsage;

tokio::task_local! {
    static TRACE: CallTrace;
}

/// What is run with a call's provider id the moment it arrives.
type IdHook = Arc<dyn Fn(&str) + Send + Sync>;

/// One call's trace, shared between the call in flight and whoever accounts
/// for it.
#[derive(Clone, Default)]
pub struct CallTrace(Arc<Inner>);

#[derive(Default)]
struct Inner {
    seen: Mutex<Traced>,
    on_id: Mutex<Option<IdHook>>,
    on_job: Mutex<Option<IdHook>>,
}

/// What a call has said about itself so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Traced {
    /// The provider's own id for the call: a generation id, a response id,
    /// an interaction id, a task id.
    pub call_id: Option<String>,
    /// The usage that has arrived. `None` until any has.
    pub usage: Option<TokenUsage>,
    /// Whether [`Self::usage`] is the call's whole usage: the provider had
    /// finished the call and said what it used. A stream cut partway has
    /// only what its early events carried, which prices nothing.
    pub whole: bool,
    /// Whether the call sent its request again after a send that had gone
    /// out and came to nothing: the provider may have billed the first, and
    /// the usage the call ends with is only the second's.
    pub resent: bool,
    /// Whether the provider began to answer: it accepted the request and
    /// opened a reply. An error that arrives after that is not a refusal,
    /// whatever status it names.
    pub began: bool,
    /// The provider's id for the job the call runs, for a provider that
    /// takes a job and is polled. It names the call in the run's record and
    /// is never looked up: a job is not a call a provider keeps a bill for.
    pub job: Option<String>,
}

impl fmt::Debug for CallTrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CallTrace").field(&self.seen()).finish()
    }
}

impl CallTrace {
    /// An empty trace.
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `hook` with the provider's id for the call when it arrives, so a
    /// caller that records the id does so at once and not when the call
    /// ends.
    pub fn on_call_id(&self, hook: impl Fn(&str) + Send + Sync + 'static) {
        *leviath_core::sync::lock(&self.0.on_id) = Some(Arc::new(hook));
    }

    /// Run `hook` with the id of each job the call submits or picks up, the
    /// moment it does, so a caller that keeps the job against the call
    /// knows which call is on it while the call is still out.
    pub fn on_job(&self, hook: impl Fn(&str) + Send + Sync + 'static) {
        *leviath_core::sync::lock(&self.0.on_job) = Some(Arc::new(hook));
    }

    /// Run `call` with this trace as the one its provider notes into.
    pub fn scope<F: Future>(&self, call: F) -> impl Future<Output = F::Output> {
        TRACE.scope(self.clone(), call)
    }

    /// What the call has said so far.
    pub fn seen(&self) -> Traced {
        leviath_core::sync::lock(&self.0.seen).clone()
    }

    /// Keep `id` as the call's, when it has none yet, and run the hook.
    fn call_id(&self, id: &str) {
        {
            let mut seen = leviath_core::sync::lock(&self.0.seen);
            if seen.call_id.is_some() {
                return;
            }
            seen.call_id = Some(id.to_string());
        }
        let hook = leviath_core::sync::lock(&self.0.on_id).clone();
        if let Some(hook) = hook {
            hook(id);
        }
    }

    /// Keep `usage` as what the call has used so far, whole or not. Usage
    /// already known to be whole is never replaced by a part.
    fn usage(&self, usage: &TokenUsage, whole: bool) {
        let mut seen = leviath_core::sync::lock(&self.0.seen);
        if seen.whole && !whole {
            return;
        }
        seen.usage = Some(usage.clone());
        seen.whole = whole;
    }
}

impl CallTrace {
    /// Mark the call as having sent its request more than once.
    fn resent(&self) {
        leviath_core::sync::lock(&self.0.seen).resent = true;
    }

    /// Mark the provider as having begun to answer.
    fn began(&self) {
        leviath_core::sync::lock(&self.0.seen).began = true;
    }

    /// Keep `id` as the job the call runs, and run the hook. A later job
    /// replaces it: a call that runs two is known by the one it is on.
    fn job(&self, id: &str) {
        leviath_core::sync::lock(&self.0.seen).job = Some(id.to_string());
        let hook = leviath_core::sync::lock(&self.0.on_job).clone();
        if let Some(hook) = hook {
            hook(id);
        }
    }
}

/// The trace of the call this runs in, when it runs in one.
fn current() -> Option<CallTrace> {
    TRACE.try_with(CallTrace::clone).ok()
}

/// Note the provider's own id for the call in flight. The first one noted
/// stands: a call that makes several requests is known by its first.
pub fn note_call_id(id: &str) {
    if let Some(trace) = current().filter(|_| !id.is_empty()) {
        trace.call_id(id);
    }
}

/// Note that the call in flight is sending its request again after a send
/// that failed, where `went_out` is whether that send reached the provider:
/// `Some(false)` when it certainly did not, which is nothing to note. What a
/// send that went out was billed is not in anything the call will report.
pub fn note_resent(went_out: Option<bool>) {
    if let Some(trace) = current().filter(|_| went_out != Some(false)) {
        trace.resent();
    }
}

/// Note that the provider has begun to answer the call in flight: its reply
/// is open, and whatever comes next comes from inside it.
pub fn note_began() {
    if let Some(trace) = current() {
        trace.began();
    }
}

/// Note the provider's id for the job the call in flight runs.
pub fn note_job(id: &str) {
    if let Some(trace) = current() {
        trace.job(id);
    }
}

/// Note the usage the call in flight has reported so far, as a running
/// total that is not yet the whole call's.
pub fn note_usage(usage: &TokenUsage) {
    if let Some(trace) = current() {
        trace.usage(usage, false);
    }
}

/// Note the call's whole usage: the provider finished and said what it
/// used, whatever becomes of the reply after that.
pub fn note_whole_usage(usage: &TokenUsage) {
    if let Some(trace) = current() {
        trace.usage(usage, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn notes_made_inside_a_scope_reach_the_trace() {
        let trace = CallTrace::new();
        trace
            .scope(async {
                note_call_id("gen-1");
                note_usage(&TokenUsage::new(10, 0, 0, 0));
            })
            .await;
        let seen = trace.seen();
        assert_eq!(seen.call_id.as_deref(), Some("gen-1"));
        assert_eq!(seen.usage.map(|u| u.prompt_tokens), Some(10));
        assert!(!seen.whole);
        assert!(format!("{trace:?}").contains("gen-1"));
    }

    #[tokio::test]
    async fn the_first_id_stands_and_an_empty_one_is_no_id() {
        let trace = CallTrace::new();
        let ran = Arc::new(AtomicUsize::new(0));
        let counted = ran.clone();
        trace.on_call_id(move |id| {
            assert_eq!(id, "resp_1");
            counted.fetch_add(1, Ordering::SeqCst);
        });
        trace
            .scope(async {
                note_call_id("");
                note_call_id("resp_1");
                note_call_id("resp_2");
            })
            .await;
        assert_eq!(trace.seen().call_id.as_deref(), Some("resp_1"));
        assert_eq!(ran.load(Ordering::SeqCst), 1, "the hook runs once");
    }

    #[tokio::test]
    async fn whole_usage_is_not_replaced_by_a_part() {
        let trace = CallTrace::new();
        trace
            .scope(async {
                note_usage(&TokenUsage::new(1, 0, 0, 0));
                note_whole_usage(&TokenUsage::new(5, 0, 0, 7));
                note_usage(&TokenUsage::new(9, 0, 0, 9));
            })
            .await;
        let seen = trace.seen();
        assert!(seen.whole);
        assert_eq!(seen.usage.map(|u| u.completion_tokens), Some(7));
    }

    #[tokio::test]
    async fn a_send_made_again_is_remembered() {
        let trace = CallTrace::new();
        assert!(!trace.seen().resent);
        // A send that never left is not one the provider could have billed.
        trace.scope(async { note_resent(Some(false)) }).await;
        assert!(!trace.seen().resent);
        trace.scope(async { note_resent(None) }).await;
        assert!(trace.seen().resent);
        let sure = CallTrace::new();
        sure.scope(async { note_resent(Some(true)) }).await;
        assert!(sure.seen().resent);
    }

    #[tokio::test]
    async fn a_reply_that_opened_and_a_job_are_remembered() {
        let trace = CallTrace::new();
        assert!(!trace.seen().began);
        assert_eq!(trace.seen().job, None);
        trace
            .scope(async {
                note_began();
                note_job("job-1");
                note_job("job-2");
            })
            .await;
        let seen = trace.seen();
        assert!(seen.began);
        assert_eq!(seen.job.as_deref(), Some("job-2"), "the one it is on");
        assert_eq!(seen.call_id, None, "a job is not a call to look up");

        // Whoever keeps the job against the call hears of each as it is
        // noted, while the call is still out.
        let heard = Arc::new(Mutex::new(Vec::new()));
        let hears = heard.clone();
        let watched = CallTrace::new();
        watched.on_job(move |job| leviath_core::sync::lock(&hears).push(job.to_string()));
        watched
            .scope(async {
                note_job("job-1");
                note_job("job-2");
            })
            .await;
        assert_eq!(*leviath_core::sync::lock(&heard), ["job-1", "job-2"]);
    }

    #[tokio::test]
    async fn outside_a_scope_a_note_goes_nowhere() {
        note_resent(Some(true));
        note_began();
        note_job("job-1");
        note_call_id("gen-1");
        note_usage(&TokenUsage::new(1, 0, 0, 0));
        note_whole_usage(&TokenUsage::new(1, 0, 0, 0));
        assert_eq!(CallTrace::new().seen(), Traced::default());
    }
}
