//! Tests for [`super`].

use super::*;
use std::sync::atomic::AtomicUsize;

/// A 404 for a job, as `check_http_response` reports one.
fn gone() -> ProviderError {
    ProviderError::from_http(reqwest::StatusCode::NOT_FOUND, "no such task", None)
}

/// Counts submissions, answering each with the next id.
struct Remote {
    submitted: AtomicUsize,
}

impl Remote {
    fn new() -> Self {
        Self {
            submitted: AtomicUsize::new(0),
        }
    }

    async fn submit(&self) -> Result<String> {
        let n = self.submitted.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(format!("job-{n}"))
    }

    fn submitted(&self) -> usize {
        self.submitted.load(Ordering::SeqCst)
    }
}

/// Outside a call's scope there is nothing to resume and nothing recorded:
/// every job is submitted.
#[tokio::test]
async fn with_no_log_every_job_is_submitted() {
    let remote = Remote::new();
    let ran = submit_or_resume("step", || remote.submit(), |id| async move { Ok(id) })
        .await
        .unwrap();
    assert_eq!((ran.id.as_str(), ran.value.as_str()), ("job-1", "job-1"));
    assert!(ran.note.is_none());
    assert_eq!(remote.submitted(), 1);
}

/// A job is recorded the moment it is submitted, before it is waited on, and
/// the run is woken to record it.
#[tokio::test]
async fn a_submitted_job_is_recorded_before_it_is_waited_on() {
    let remote = Remote::new();
    let log = JobLog::default();
    let wake = Arc::new(Notify::new());
    log.wake_with(wake.clone());
    let seen = log.clone();
    let ran = log
        .scope(submit_or_resume(
            "meshy/rig",
            || remote.submit(),
            |id| {
                let held = seen.jobs();
                async move {
                    assert_eq!(held["meshy/rig"], id, "recorded before the wait");
                    Ok(())
                }
            },
        ))
        .await
        .unwrap();
    assert_eq!(ran.id, "job-1");
    assert_eq!(log.version(), 1);
    // The wake was stored as a permit, so this returns at once.
    wake.notified().await;
    assert_eq!(format!("{log:?}"), r#"JobLog({"meshy/rig": "job-1"})"#);
}

/// A call made again, with the job it submitted before in its log, polls
/// that job: nothing is submitted.
#[tokio::test]
async fn a_call_made_again_polls_the_job_it_submitted() {
    let remote = Remote::new();
    let log = JobLog::new([("step".to_string(), "job-7".to_string())].into());
    let ran = log
        .scope(submit_or_resume(
            "step",
            || remote.submit(),
            |id| async move { Ok(format!("{id} done")) },
        ))
        .await
        .unwrap();
    assert_eq!(ran.value, "job-7 done");
    assert_eq!(
        remote.submitted(),
        0,
        "one submit in all, before the restart"
    );
    assert_eq!(log.version(), 0, "nothing new to record");
}

/// A job the provider no longer has is submitted again, and the call says so.
#[tokio::test]
async fn a_job_that_is_gone_is_submitted_again_and_said() {
    // With a subscriber listening, so the warning it logs is made.
    let _guard = crate::test_support::always_on_tracing_guard();
    let remote = Remote::new();
    let log = JobLog::new([("step".to_string(), "job-old".to_string())].into());
    let ran = log
        .scope(submit_or_resume(
            "step",
            || remote.submit(),
            |id| async move {
                match id.as_str() {
                    "job-old" => Err(gone()),
                    _ => Ok(id),
                }
            },
        ))
        .await
        .unwrap();
    assert_eq!(ran.value, "job-1");
    assert_eq!(remote.submitted(), 1);
    assert!(
        ran.note.as_deref().is_some_and(|n| n.contains("job-old")),
        "{:?}",
        ran.note
    );
    assert_eq!(log.jobs()["step"], "job-1");
}

/// A job that failed is forgotten, so the call made again submits afresh;
/// one whose polling broke on the way is kept for it to pick back up.
#[tokio::test]
async fn a_failed_job_is_forgotten_and_a_broken_poll_is_not() {
    let remote = Remote::new();
    let log = JobLog::default();
    let failed = log
        .scope(submit_or_resume(
            "step",
            || remote.submit(),
            |_| async { Err::<(), _>(ProviderError::Other("task failed".into())) },
        ))
        .await;
    assert!(failed.is_err());
    assert!(log.jobs().is_empty(), "a failed job is not picked back up");

    let broken = log
        .scope(submit_or_resume(
            "step",
            || remote.submit(),
            |_| async { Err::<(), _>(ProviderError::RequestFailed("reset".into())) },
        ))
        .await;
    assert!(broken.is_err());
    assert_eq!(log.jobs()["step"], "job-2", "kept for the call made again");

    // The same, for a job the log already named.
    let resumed = log
        .scope(submit_or_resume(
            "step",
            || remote.submit(),
            |_| async { Err::<(), _>(ProviderError::Other("task failed".into())) },
        ))
        .await;
    assert!(resumed.is_err());
    assert!(log.jobs().is_empty());
    assert_eq!(remote.submitted(), 2);
}

/// A submit that fails records nothing.
#[tokio::test]
async fn a_submit_that_fails_records_nothing() {
    let log = JobLog::default();
    let refused = log
        .scope(submit_or_resume(
            "step",
            || async { Err(ProviderError::ApiError("no".into())) },
            |id| async move { Ok(id) },
        ))
        .await;
    assert!(refused.is_err());
    assert_eq!(log.version(), 0);
}

/// A transport failure of the kind that, alone, says nothing left this
/// machine.
fn refused() -> ProviderError {
    ProviderError::labelled(
        FailureKind::ConnectionRefused,
        "polling",
        "nothing listening",
    )
}

/// Once there is a job, whatever fails says its request was sent: the
/// provider has the job even when the poll that failed never connected. A
/// first submit that fails answers for itself.
#[tokio::test]
async fn what_fails_behind_a_job_says_its_request_was_sent() {
    assert_eq!(refused().request_sent(), Some(false));
    let remote = Remote::new();

    // The submit itself never connects: nothing went out.
    let unsent = submit_or_resume(
        "step",
        || async { Err(refused()) },
        |id| async move { Ok(id) },
    )
    .await
    .unwrap_err();
    assert_eq!(unsent.request_sent(), Some(false));

    // Submitted in this call, then the poll never connects.
    let polled = submit_or_resume(
        "step",
        || remote.submit(),
        |_| async { Err::<(), _>(refused()) },
    )
    .await
    .unwrap_err();
    assert_eq!(polled.request_sent(), Some(true));
    assert_eq!(polled.failure_kind(), Some(FailureKind::ConnectionRefused));

    // Submitted by an earlier call and only waited on in this one.
    let log = JobLog::new([("step".to_string(), "job-old".to_string())].into());
    let resumed = log
        .scope(submit_or_resume(
            "step",
            || remote.submit(),
            |_| async { Err::<(), _>(refused()) },
        ))
        .await
        .unwrap_err();
    assert_eq!(resumed.request_sent(), Some(true));
    assert_eq!(remote.submitted(), 1, "the job it had was waited on");

    // The old job is gone, which its poll was answered to say, and the
    // submit made in its place never connects.
    let resubmitted = log
        .scope(submit_or_resume(
            "step",
            || async { Err(refused()) },
            |_| async { Err::<(), _>(gone()) },
        ))
        .await
        .unwrap_err();
    assert_eq!(resubmitted.request_sent(), Some(true));
}

/// Clearing a log forgets its jobs and counts as a change only when it held
/// some.
#[test]
fn clearing_forgets_every_job() {
    let log = JobLog::default();
    log.clear();
    assert_eq!(log.version(), 0, "nothing to forget");
    let log = JobLog::new([("a".to_string(), "1".to_string())].into());
    log.clear();
    assert!(log.jobs().is_empty());
    assert_eq!(log.version(), 1);
}

/// A run that waits on some of its jobs no longer keeps the ones it is
/// told to, by id, and counts a change only when it forgot one.
#[test]
fn keeping_some_jobs_forgets_the_rest() {
    let log = JobLog::new(
        [
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
        ]
        .into(),
    );
    log.keep(&|_| true);
    assert_eq!(log.version(), 0, "nothing forgotten");
    log.keep(&|id| id == "2");
    assert_eq!(log.jobs(), [("b".to_string(), "2".to_string())].into());
    assert_eq!(log.version(), 1);
}

#[test]
fn notes_follow_the_summary() {
    assert_eq!(noted("made it".into(), [None, None]), "made it");
    assert_eq!(
        noted(
            "made it".into(),
            [Some("one".into()), None, Some("two".into())]
        ),
        "made it\none\ntwo"
    );
}

/// A job still running when its call stopped waiting is kept for the call
/// made again, where one the provider said failed is forgotten; and a job is
/// the id its call is known by.
#[tokio::test]
async fn a_job_that_outlasts_the_wait_is_kept_and_names_its_call() {
    let log = JobLog::default();
    let trace = crate::CallTrace::new();
    let waited: Result<Ran<()>> = trace
        .scope(log.scope(submit_or_resume(
            "p/m/video",
            || async { Ok("job-1".to_string()) },
            |_id| async { Err(still_running("it did not finish".into())) },
        )))
        .await;
    let error = waited.unwrap_err();
    assert_eq!(error.request_sent(), Some(true));
    assert_eq!(
        log.jobs().get("p/m/video").map(String::as_str),
        Some("job-1")
    );
    // The job names the call, and is not an id to look a bill up by.
    assert_eq!(trace.seen().job.as_deref(), Some("job-1"));
    assert_eq!(trace.seen().call_id, None);

    // Made again, the call picks the same job up and is known by it.
    let again = crate::CallTrace::new();
    let ran = again
        .scope(log.scope(submit_or_resume(
            "p/m/video",
            || async { panic!("the job it holds is not submitted again") },
            |id| async move { Ok(id) },
        )))
        .await
        .unwrap();
    assert_eq!(ran.value, "job-1");
    assert_eq!(again.seen().job.as_deref(), Some("job-1"));

    // A job the provider said failed is forgotten.
    let failed: Result<Ran<()>> = log
        .scope(submit_or_resume(
            "p/m/video",
            || async { panic!("it holds one") },
            |_id| async { Err(ProviderError::Other("the job failed".into())) },
        ))
        .await;
    assert!(failed.is_err());
    assert!(log.jobs().is_empty());
}
