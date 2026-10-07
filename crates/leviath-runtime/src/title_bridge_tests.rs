//! A title call driven to its end the way the world drives it.

use super::*;
use tokio::sync::mpsc;

/// Drive `job` to its end as the world does, without a world: each trip is
/// made and settled with [`TitleCall::settle`], and a waiting call is sent
/// again once its due time comes, until the call is done. The finished
/// outcome goes to `results` after the call lets go of its permit.
///
/// The title lane's own tests drive their jobs through this, so the schedule
/// they assert is the one the world keeps.
pub(crate) async fn run_title_job(
    job: TitleJob,
    policy: crate::inference_bridge::RetryPolicy,
    results: UnboundedSender<TitleOutcome>,
    wake: Arc<Notify>,
) {
    let entity = job.entity;
    let TitleJob {
        provider,
        provider_name,
        model,
        request,
        permit,
        ..
    } = job;
    let mut call = TitleCall {
        provider,
        provider_name,
        model,
        request: Arc::new(request),
        _permit: permit,
        clock: RetryClock::start(&policy),
        policy,
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut guard = true;
    loop {
        run_title_attempt(
            call.attempt(entity, guard),
            tx.clone(),
            Arc::new(Notify::new()),
        )
        .await;
        guard = false;
        let outcome = rx.try_recv().expect("a trip always reports");
        if call.settle(&outcome) == Next::Done {
            drop(call);
            let _ = results.send(outcome);
            wake.notify_one();
            return;
        }
        let due = call.clock.due_at().expect("a waiting call has a due time");
        tokio::time::sleep_until(due).await;
        if call.clock.take_due(tokio::time::Instant::now()) == Some(Due::Expired) {
            let outcome = call.timed_out(entity);
            drop(call);
            let _ = results.send(outcome);
            wake.notify_one();
            return;
        }
    }
}
