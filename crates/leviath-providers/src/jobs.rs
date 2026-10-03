//! The remote jobs a model call has submitted, so a call made again polls the
//! job it already paid for instead of submitting another.
//!
//! Some providers do not answer a call: they take a job (a Meshy task, a
//! video), hand back its id, and are polled until it is done. A daemon that
//! stops while one runs comes back and makes the call again, which on its own
//! would submit, and pay for, the same job a second time. So the runtime runs
//! each call inside [`JobLog::scope`], with a log seeded from the jobs its
//! run recorded, and keeps on the run whatever the provider writes to it. A
//! provider submits through `submit_or_resume`, which polls the job the log
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
        self.0
            .jobs
            .lock()
            .expect("the job log is never held across a panic")
            .clone()
    }

    /// How many times it has changed.
    pub fn version(&self) -> u64 {
        self.0.version.load(Ordering::Acquire)
    }

    /// Wake `wake` whenever it changes.
    pub fn wake_with(&self, wake: Arc<Notify>) {
        *self
            .0
            .wake
            .lock()
            .expect("the job log is never held across a panic") = Some(wake);
    }

    /// Forget every job: the call they belonged to is over.
    pub fn clear(&self) {
        self.change(|jobs| !std::mem::take(jobs).is_empty());
    }

    /// Run `call` with this log as the one its provider submits through.
    pub async fn scope<F: Future>(&self, call: F) -> F::Output {
        JOBS.scope(self.clone(), call).await
    }

    fn get(&self, step: &str) -> Option<String> {
        self.0
            .jobs
            .lock()
            .expect("the job log is never held across a panic")
            .get(step)
            .cloned()
    }

    fn put(&self, step: &str, id: &str) {
        self.change(|jobs| {
            jobs.insert(step.to_string(), id.to_string());
            true
        });
    }

    fn forget(&self, step: &str) {
        self.change(|jobs| jobs.remove(step).is_some());
    }

    /// Apply `edit`, and when it says it changed something, say so.
    fn change(&self, edit: impl FnOnce(&mut BTreeMap<String, String>) -> bool) {
        let changed = edit(
            &mut self
                .0
                .jobs
                .lock()
                .expect("the job log is never held across a panic"),
        );
        if changed {
            self.0.version.fetch_add(1, Ordering::AcqRel);
            if let Some(wake) = self
                .0
                .wake
                .lock()
                .expect("the job log is never held across a panic")
                .as_ref()
            {
                wake.notify_one();
            }
        }
    }
}

/// The log of the call this runs in, when it runs in one.
fn current() -> Option<JobLog> {
    JOBS.try_with(JobLog::clone).ok()
}

/// A job run to its end through [`submit_or_resume`].
#[derive(Debug)]
pub(crate) struct Ran<T> {
    /// Its id.
    pub(crate) id: String,
    /// What waiting on it came back with.
    pub(crate) value: T,
    /// When the job this call had already submitted was gone and a new one
    /// was submitted in its place, the line that says so.
    pub(crate) note: Option<String>,
}

/// Run one remote job, `step` naming it within the call: wait on the job
/// this call already submitted for that step, or `submit` one, record its id,
/// and wait on that. `wait` polls a job by id to its end.
///
/// A job the provider no longer has (it answers 404 for it) is submitted
/// again, and the result's `note` says so. A job that failed, or that ran
/// past its deadline, is forgotten, so the call made again submits afresh;
/// one whose polling failed on the way (a dropped connection, a busy
/// provider) is kept, so the call made again picks it back up.
pub(crate) async fn submit_or_resume<T, S, SF, W, WF>(
    step: &str,
    submit: S,
    mut wait: W,
) -> Result<Ran<T>>
where
    S: FnOnce() -> SF,
    SF: Future<Output = Result<String>>,
    W: FnMut(String) -> WF,
    WF: Future<Output = Result<T>>,
{
    let log = current();
    let mut note = None;
    if let Some(id) = log.as_ref().and_then(|l| l.get(step)) {
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
    let id = submit().await?;
    if let Some(log) = &log {
        log.put(step, &id);
    }
    let value = settle(log.as_ref(), step, wait(id.clone()).await)?;
    Ok(Ran { id, value, note })
}

/// `waited`, forgetting the job when it ended without a result: a failed job
/// or one past its deadline is never picked back up.
fn settle<T>(log: Option<&JobLog>, step: &str, waited: Result<T>) -> Result<T> {
    if let (Some(log), Err(ProviderError::Other(_))) = (log, &waited) {
        log.forget(step);
    }
    waited
}

/// `summary`, followed by each note.
pub(crate) fn noted(summary: String, notes: impl IntoIterator<Item = Option<String>>) -> String {
    notes
        .into_iter()
        .flatten()
        .fold(summary, |text, note| format!("{text}\n{note}"))
}

#[cfg(test)]
#[path = "jobs_tests.rs"]
mod tests;
