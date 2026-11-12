//! The remote jobs a model call has submitted, so a call made again polls the
//! job it already paid for instead of submitting another.
//!
//! Some providers do not answer a call: they take a job (a Meshy task, a
//! video), hand back its id, and are polled until it is done. A daemon that
//! stops while one runs comes back and makes the call again, which on its own
//! would submit, and pay for, the same job a second time. So the runtime runs
//! each call inside [`JobLog::scope`], with a log seeded from the jobs its
//! run recorded, and keeps on the run whatever the provider writes to it. A
//! provider submits through [`submit_or_resume`], which polls the job the log
//! already names for that step and records a new one the moment it is
//! submitted.
//!
//! Outside a scope (a provider called directly, a test, a one-off call) there
//! is no log: every job is submitted, as it always was.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::failure::FailureKind;
use crate::provider::{ProviderError, Result};

tokio::task_local! {
    static JOBS: JobLog;
}

/// The jobs one run's model call has submitted, by the call's name for each
/// step (`meshy/text-to-3d/preview`), shared between the call in flight and
/// the run that records it.
#[derive(Clone, Default)]
pub struct JobLog(Arc<Inner>);

#[derive(Default)]
struct Inner {
    jobs: Mutex<BTreeMap<String, String>>,
    /// Bumped on every change, so the run can tell it has something new to
    /// record without comparing the whole map.
    version: AtomicU64,
    /// Woken on every change, so the run records a new job at once rather
    /// than when the call ends.
    wake: Mutex<Option<Arc<Notify>>>,
}

impl fmt::Debug for JobLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("JobLog").field(&self.jobs()).finish()
    }
}

impl JobLog {
    /// A log holding `jobs`, the ones a run recorded before.
    pub fn new(jobs: BTreeMap<String, String>) -> Self {
        Self(Arc::new(Inner {
            jobs: Mutex::new(jobs),
            ..Inner::default()
        }))
    }

    /// The jobs it holds, by step.
    pub fn jobs(&self) -> BTreeMap<String, String> {
        self.held().clone()
    }

    /// How many times it has changed.
    pub fn version(&self) -> u64 {
        self.0.version.load(Ordering::Acquire)
    }

    /// Wake `wake` whenever it changes.
    pub fn wake_with(&self, wake: Arc<Notify>) {
        *leviath_core::sync::lock(&self.0.wake) = Some(wake);
    }

    /// Forget every job: the call they belonged to is over.
    pub fn clear(&self) {
        let held = std::mem::take(&mut *self.held());
        if !held.is_empty() {
            self.changed();
        }
    }

    /// Forget every job whose id `keep` does not answer for: the run that
    /// held them waits on them no longer.
    pub fn keep(&self, keep: &dyn Fn(&str) -> bool) {
        let before = self.held().len();
        self.held().retain(|_, id| keep(id));
        if self.held().len() != before {
            self.changed();
        }
    }

    /// Run `call` with this log as the one its provider submits through.
    pub fn scope<F: Future>(&self, call: F) -> impl Future<Output = F::Output> {
        JOBS.scope(self.clone(), call)
    }

    fn get(&self, step: &str) -> Option<String> {
        self.held().get(step).cloned()
    }

    fn put(&self, step: &str, id: &str) {
        self.held().insert(step.to_string(), id.to_string());
        self.changed();
    }

    fn forget(&self, step: &str) {
        self.held().remove(step);
        self.changed();
    }

    fn held(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, String>> {
        leviath_core::sync::lock(&self.0.jobs)
    }

    /// Say it changed: count it, and wake whoever records it.
    fn changed(&self) {
        self.0.version.fetch_add(1, Ordering::AcqRel);
        if let Some(wake) = leviath_core::sync::lock(&self.0.wake).as_ref() {
            wake.notify_one();
        }
    }
}

/// The log of the call this runs in, when it runs in one.
fn current() -> Option<JobLog> {
    JOBS.try_with(JobLog::clone).ok()
}

/// A job run to its end through [`submit_or_resume`].
#[derive(Debug)]
pub struct Ran<T> {
    /// Its id.
    pub id: String,
    /// What waiting on it came back with.
    pub value: T,
    /// When the job this call had already submitted was gone and a new one
    /// was submitted in its place, the line that says so.
    pub note: Option<String>,
}

/// Run one remote job, `step` naming it within the call: wait on the job
/// this call already submitted for that step, or `submit` one, record its id,
/// and wait on that. `wait` polls a job by id to its end.
///
/// A job the provider no longer has (it answers 404 for it) is submitted
/// again, and the result's `note` says so. A job the provider said failed is
/// forgotten, so the call made again submits afresh. One still running when
/// this call stopped waiting (`still_running`), or whose polling failed on
/// the way (a dropped connection, a busy provider), is kept: the provider
/// goes on running and billing it, so the call made again picks it back up
/// rather than paying for another.
///
/// Whatever fails once there is a job says its request was sent (see
/// [`ProviderError::request_sent`]): the provider has the job, whether this
/// call submitted it or an earlier one did, even when the poll that failed
/// never connected. Only a first submit that fails answers for itself.
pub async fn submit_or_resume<T, S, SF, W, WF>(step: &str, submit: S, mut wait: W) -> Result<Ran<T>>
where
    S: FnOnce() -> SF,
    SF: Future<Output = Result<String>>,
    W: FnMut(String) -> WF,
    WF: Future<Output = Result<T>>,
{
    let log = current();
    let mut note = None;
    let resumed = log.as_ref().and_then(|l| l.get(step));
    let polled = resumed.is_some();
    if let Some(id) = resumed {
        // The provider has this job, so the call is one it knows by an id.
        crate::call_trace::note_job(&id);
        match wait(id.clone()).await {
            Err(e) if e.failure_kind() == Some(FailureKind::NotFound) => {
                tracing::warn!(step, job = %id, "a job this call submitted before is gone; submitting it again");
                note = Some(format!(
                    "The job {id} submitted for {step} before the daemon restarted was gone, so it was submitted again."
                ));
            }
            waited => {
                let value = settle(log.as_ref(), step, waited)?;
                return Ok(Ran { id, value, note });
            }
        }
    }
    let id = submit().await.map_err(|e| submit_failed(polled, e))?;
    crate::call_trace::note_job(&id);
    if let Some(log) = &log {
        log.put(step, &id);
    }
    let value = settle(log.as_ref(), step, wait(id.clone()).await)?;
    Ok(Ran { id, value, note })
}

/// A failed submit's error. One made after the old job's poll was answered
/// follows a request that went out; a first submit says for itself whether
/// it did.
fn submit_failed(after_poll: bool, error: ProviderError) -> ProviderError {
    match after_poll {
        true => error.after_send(),
        false => error,
    }
}

/// The tag at the head of the error for a job this call stopped waiting on
/// while the provider was still running it.
const STILL_RUNNING: &str = "[job-running] ";

/// The error for a job that outlasted the wait for it: `why`, marked so the
/// job is kept rather than forgotten. Its request was sent.
#[cfg(any(
    test,
    feature = "openrouter",
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google"
))]
pub(crate) fn still_running(why: String) -> ProviderError {
    ProviderError::Other(format!("{STILL_RUNNING}{why}")).after_send()
}

/// Whether `error` says its job is still running at the provider.
fn is_still_running(error: &ProviderError) -> bool {
    matches!(error, ProviderError::Other(m) if m.contains(STILL_RUNNING))
}

/// `waited`, forgetting the job when the provider said it failed: a failed
/// job is never picked back up, and one still running is. Its error says a
/// request was sent, since the job it waited on was.
fn settle<T>(log: Option<&JobLog>, step: &str, waited: Result<T>) -> Result<T> {
    if let (Some(log), Err(error @ ProviderError::Other(_))) = (log, &waited)
        && !is_still_running(error)
    {
        log.forget(step);
    }
    waited.map_err(ProviderError::after_send)
}

/// `summary`, followed by each note.
#[cfg(any(
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "meshy",
    feature = "openrouter"
))]
pub(crate) fn noted(summary: String, notes: impl IntoIterator<Item = Option<String>>) -> String {
    notes
        .into_iter()
        .flatten()
        .fold(summary, |text, note| format!("{text}\n{note}"))
}

#[cfg(test)]
#[path = "jobs_tests.rs"]
mod tests;
