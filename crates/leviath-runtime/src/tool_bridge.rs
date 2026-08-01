//! The async worker side of the ECS tool stage - the sync-ECS ↔ async-I/O
//! bridge for tool execution.
//!
//! When the pipeline decides an agent's response has tool calls to run, the
//! tool-dispatch system builds a [`ToolJob`] (the agent plus a boxed async
//! closure that executes that agent's batch of calls against its own tool
//! registry / workdir / policy) and sends it to the tool lane. The lane runs the
//! batch and reports its [`ToolOutcome`] back on the results channel, waking the
//! tick loop; the tool-collect system applies the results on a later tick.
//!
//! **Concurrency**: the lane is a *semaphore*, not a pool of workers.
//! [`ToolLane::serve`] reads jobs off the channel and spawns one task per batch;
//! each task holds a permit for as long as it is executing, so
//! `max_concurrent_tools` batches run at a time.
//!
//! It used to be a fixed pool of worker tasks, and that deadlocked. Several
//! things a batch can await have no time bound at all: a tool-approval prompt, an
//! `ask_user`, a `wait_for_agent` poll that only ends when some other run
//! finishes. A worker sitting in one of those was a unit of capacity spent on
//! waiting rather than working, and a parent waiting on a child it had spawned
//! was holding capacity that child needed to finish. Eight of those and no
//! agent's tools ran again, for as long as the daemon lived (issue #191).
//!
//! A permit, unlike a worker, can be handed back in the middle of a batch. That
//! is what [`off_lane`] does, and it is what makes the deadlock impossible:
//! waiting costs the lane nothing, and the batch takes a permit again when it has
//! something to do.
//!
//! **Order**: which of two batches submitted together gets in first is not
//! fixed - they race for the permit as separate tasks. Nothing depends on it,
//! since an agent only ever has one batch in flight, and once both are actually
//! waiting the semaphore hands out permits first-come-first-served, so nothing
//! is starved either.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use bevy_ecs::entity::Entity;
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::{JoinHandle, JoinSet};

use crate::inference_pool::expect_permit;

/// The future produced by a boxed tool-execution closure: resolves to
/// `(tool_call_id, result)` pairs - the same shape the engine's tool executors
/// already return.
pub type ToolExecFuture = Pin<Box<dyn Future<Output = Vec<(String, String)>> + Send>>;

/// A boxed, per-agent tool-execution closure. Built by the dispatch system so it
/// captures that agent's own tool registry, workdir, and policy; run once by the
/// tool lane.
pub type BoxedToolExec = Box<dyn FnOnce() -> ToolExecFuture + Send>;

/// A batch of tool calls to execute for one agent.
pub struct ToolJob {
    /// The agent the calls belong to.
    pub entity: Entity,
    /// Runs the agent's batch of tool calls.
    pub exec: BoxedToolExec,
    /// Fires when the agent is cancelled, so the lane drops the batch instead of
    /// running it to completion. The agent holds the other half.
    pub cancel: crate::cancel::CancelToken,
}

/// The result of a [`ToolJob`], applied on a later tick by the tool-collect
/// system.
pub struct ToolOutcome {
    /// The agent the results belong to.
    pub entity: Entity,
    /// `(tool_call_id, result)` pairs.
    pub results: Vec<(String, String)>,
    /// Wall-clock time the whole batch took. Per-call timing would require
    /// every executor to report it through `BoxedToolExec`'s return shape, so
    /// each call in the batch shares this one figure.
    pub elapsed: std::time::Duration,
}

/// Live occupancy of the tool lane.
///
/// The lane reads an **unbounded** queue, so dispatch never blocks and a
/// saturated lane is invisible from the outside: the batches just pile up.
/// Counting what is queued, what is running, and what is parked on a wait is what
/// makes that legible instead of guesswork.
#[derive(Debug)]
pub struct ToolLaneStats {
    queued: AtomicUsize,
    busy: AtomicUsize,
    parked: AtomicUsize,
    /// The concurrency cap. Atomic because the relief valve can raise it (see
    /// [`ToolLane::relieve`]).
    workers: AtomicUsize,
}

impl ToolLaneStats {
    /// Stats for a lane that runs `workers` batches at a time.
    pub fn new(workers: usize) -> Self {
        Self {
            queued: AtomicUsize::new(0),
            busy: AtomicUsize::new(0),
            parked: AtomicUsize::new(0),
            workers: AtomicUsize::new(workers.max(1)),
        }
    }

    /// Record a batch handed to the lane.
    pub fn enqueued(&self) {
        self.queued.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a batch leaving the queue without ever running - cancelled while it
    /// waited for capacity.
    fn abandoned(&self) {
        self.queued.fetch_sub(1, Ordering::Relaxed);
    }

    /// Record a batch taking a permit: it leaves the queue and occupies the lane.
    fn started(&self) {
        self.queued.fetch_sub(1, Ordering::Relaxed);
        self.busy.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a batch releasing its permit, however it ended.
    fn finished(&self) {
        self.busy.fetch_sub(1, Ordering::Relaxed);
    }

    /// Record a running batch stepping off the lane to wait for something
    /// unbounded: it stops occupying the lane and starts being parked.
    fn began_park(&self) {
        self.busy.fetch_sub(1, Ordering::Relaxed);
        self.parked.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a parked batch that took a permit again and is running.
    fn resumed(&self) {
        self.parked.fetch_sub(1, Ordering::Relaxed);
        self.busy.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a parked batch that was dropped where it stood, without ever
    /// taking a permit again.
    fn ended_park(&self) {
        self.parked.fetch_sub(1, Ordering::Relaxed);
    }

    /// Batches waiting for lane capacity.
    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    /// Batches holding a permit and running.
    pub fn busy(&self) -> usize {
        self.busy.load(Ordering::Relaxed)
    }

    /// Batches parked on an unbounded wait, holding no capacity.
    pub fn parked(&self) -> usize {
        self.parked.load(Ordering::Relaxed)
    }

    /// The lane's concurrency cap.
    pub fn workers(&self) -> usize {
        self.workers.load(Ordering::Relaxed)
    }

    /// Raise the cap by `extra`, to match permits added to the semaphore.
    fn widen(&self, extra: usize) {
        self.workers.fetch_add(extra, Ordering::Relaxed);
    }

    /// Whether every unit of capacity is taken and batches are waiting behind
    /// them.
    #[must_use]
    pub fn is_saturated(&self) -> bool {
        self.busy() >= self.workers() && self.queued() > 0
    }
}

/// The tool lane: the capacity that bounds how many batches execute at once, and
/// the plumbing a batch needs to report its outcome.
pub struct ToolLane {
    /// One permit per concurrent batch.
    permits: Arc<Semaphore>,
    /// Shared with the world so `lane_snapshot` can read it.
    stats: Arc<ToolLaneStats>,
    /// Where finished batches report.
    results: UnboundedSender<ToolOutcome>,
    /// Notified whenever a batch finishes or frees capacity, so the tick loop
    /// re-drives.
    wake: Arc<Notify>,
    /// Where batch tasks are spawned.
    runtime: Handle,
}

impl ToolLane {
    /// Build a lane that runs `concurrency` batches at a time (clamped to at
    /// least one, matching [`ToolLaneStats::new`]).
    pub fn new(
        runtime: Handle,
        results: UnboundedSender<ToolOutcome>,
        wake: Arc<Notify>,
        concurrency: usize,
        stats: Arc<ToolLaneStats>,
    ) -> Arc<Self> {
        Arc::new(Self {
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
            stats,
            results,
            wake,
            runtime,
        })
    }

    /// Start serving `jobs`. The returned handle completes once the job channel
    /// closes (the world is shutting down) **and** every batch it started has
    /// finished, so awaiting it drains the lane.
    pub fn serve(self: &Arc<Self>, jobs: UnboundedReceiver<ToolJob>) -> JoinHandle<()> {
        let lane = self.clone();
        self.runtime.clone().spawn(serve_lane(lane, jobs))
    }

    /// Add `extra` permits, widening the lane for good.
    ///
    /// The relief valve under a lane that has stopped draining: handing out more
    /// capacity lets the queued batches run without cancelling anything. Returns
    /// how many were added.
    pub fn relieve(&self, extra: usize) -> usize {
        if extra == 0 {
            return 0;
        }
        self.permits.add_permits(extra);
        self.stats.widen(extra);
        extra
    }

    /// The lane's occupancy counters.
    pub fn stats(&self) -> &Arc<ToolLaneStats> {
        &self.stats
    }
}

/// Read jobs off the channel, spawning one task per batch, then wait for the
/// batches still running once the channel closes.
async fn serve_lane(lane: Arc<ToolLane>, mut jobs: UnboundedReceiver<ToolJob>) {
    let mut batches = JoinSet::new();
    loop {
        tokio::select! {
            job = jobs.recv() => match job {
                Some(job) => {
                    batches.spawn_on(run_batch(lane.clone(), job), &lane.runtime);
                }
                None => break, // channel closed → shutting down
            },
            // Reap finished batches as we go so the set can't grow without
            // bound over a long-lived daemon. Disabled while empty, since
            // `join_next` on an empty set is instantly ready and would spin.
            Some(_) = batches.join_next(), if !batches.is_empty() => {}
        }
    }
    while batches.join_next().await.is_some() {}
}

/// Run one batch: wait for capacity, execute it under a [`LaneTicket`], and
/// report the outcome.
async fn run_batch(lane: Arc<ToolLane>, job: ToolJob) {
    let ToolJob {
        entity,
        exec,
        cancel,
    } = job;
    // A cancel while the batch is still queued drops it without ever running -
    // the same bargain the executing case makes below, one step earlier.
    let permit = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            lane.stats.abandoned();
            return;
        }
        permit = lane.permits.clone().acquire_owned() => expect_permit(permit),
    };
    lane.stats.started();
    let ticket = Arc::new(LaneTicket::new(lane.clone(), permit));
    let started = std::time::Instant::now();
    // A cancelled agent's batch is dropped rather than run to completion. This
    // is what hands the capacity back: several of the things a batch can await
    // are unbounded, so without this a cancelled agent would keep occupying the
    // lane until whatever it was waiting for answered.
    let out = LANE_TICKET
        .scope(ticket, async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                out = exec() => Some(out),
            }
        })
        .await;
    // The ticket is dropped with the scope above, so the permit is already back
    // and the loop already woken by the time the outcome goes out.
    let Some(out) = out else { return };
    // Harmless no-op if the collect side has gone away.
    let _ = lane.results.send(ToolOutcome {
        entity,
        results: out,
        elapsed: started.elapsed(),
    });
    lane.wake.notify_one();
}

tokio::task_local! {
    /// The running batch's claim on the lane, readable from anywhere inside it.
    ///
    /// A task-local rather than an argument threaded through [`BoxedToolExec`]:
    /// the waits that need it are several layers down inside the tool service,
    /// and passing a ticket to every executor - including the many that never
    /// wait on anything - would put a concurrency detail in the signature of
    /// every `ToolService` implementation.
    static LANE_TICKET: Arc<LaneTicket>;
}

/// A batch's claim on the tool lane.
///
/// Holds a permit while the batch is executing and gives it up around an
/// unbounded wait, so a batch parked on a person or on another run costs the lane
/// nothing. Dropping it releases whatever it is holding.
struct LaneTicket {
    lane: Arc<ToolLane>,
    /// The permit, absent exactly while the batch is parked.
    permit: std::sync::Mutex<Option<OwnedSemaphorePermit>>,
    /// Whether this ticket is currently counted as parked rather than busy.
    parked: AtomicBool,
}

impl LaneTicket {
    fn new(lane: Arc<ToolLane>, permit: OwnedSemaphorePermit) -> Self {
        Self {
            lane,
            permit: std::sync::Mutex::new(Some(permit)),
            parked: AtomicBool::new(false),
        }
    }

    /// Give the permit up and start counting as parked.
    fn release(&self) {
        let held = self.take_permit();
        // Release first, wake second, for the reason `InferencePermit::drop`
        // spells out: the other order lets the woken tick re-check the lane
        // while this permit is still held.
        drop(held);
        self.parked.store(true, Ordering::Relaxed);
        self.lane.stats.began_park();
        self.lane.wake.notify_one();
    }

    /// Take a permit again, waiting for one if the lane is full. Ordinary
    /// backpressure: nothing is held while we wait, so the batches ahead can
    /// always finish.
    async fn reacquire(&self) {
        let permit = expect_permit(self.lane.permits.clone().acquire_owned().await);
        // Stop counting as parked only once the permit is actually in hand, so
        // a ticket dropped mid-wait is accounted for as the parked batch it is.
        self.lane.stats.resumed();
        self.parked.store(false, Ordering::Relaxed);
        *self
            .permit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(permit);
    }

    fn take_permit(&self) -> Option<OwnedSemaphorePermit> {
        self.permit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

impl Drop for LaneTicket {
    fn drop(&mut self) {
        // Release first, wake second - see `release`.
        drop(self.take_permit());
        match self.parked.load(Ordering::Relaxed) {
            true => self.lane.stats.ended_park(),
            false => self.lane.stats.finished(),
        }
        self.lane.wake.notify_one();
    }
}

/// Await something with no time bound without holding the tool lane.
///
/// The lane permit is handed back before `fut` is polled and taken again before
/// this returns, so a batch waiting on a person (a tool-approval prompt, an
/// `ask_user`) or on another run (`wait_for_agent`) occupies no capacity while it
/// waits. That is what stops a lane full of waiters from starving the very runs
/// they are waiting for (issue #191).
///
/// Outside a lane task - the embedded runtime, tests driving an executor
/// directly - there is no ticket and this is just `fut.await`.
pub async fn off_lane<T>(fut: impl Future<Output = T>) -> T {
    let Ok(ticket) = LANE_TICKET.try_with(Arc::clone) else {
        return fut.await;
    };
    ticket.release();
    let out = fut.await;
    ticket.reacquire().await;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// Everything a test lane needs: the lane itself, the job sender, and the
    /// outcome receiver.
    struct Harness {
        lane: Arc<ToolLane>,
        /// Taken by `drain`, which closes the lane by dropping it.
        jobs: Option<UnboundedSender<ToolJob>>,
        outcomes: mpsc::UnboundedReceiver<ToolOutcome>,
        serving: Option<JoinHandle<()>>,
        stats: Arc<ToolLaneStats>,
    }

    impl Harness {
        fn new(concurrency: usize) -> Self {
            let (jobs, job_rx) = mpsc::unbounded_channel();
            let (result_tx, outcomes) = mpsc::unbounded_channel();
            let stats = Arc::new(ToolLaneStats::new(concurrency));
            let lane = ToolLane::new(
                Handle::current(),
                result_tx,
                Arc::new(Notify::new()),
                concurrency,
                stats.clone(),
            );
            let serving = lane.serve(job_rx);
            Self {
                lane,
                jobs: Some(jobs),
                outcomes,
                serving: Some(serving),
                stats,
            }
        }

        /// Hand a batch to the lane, counting it the way `dispatch_tools` does.
        fn submit(&self, job: ToolJob) {
            self.stats.enqueued();
            self.sender().send(job).expect("the lane is serving");
        }

        fn sender(&self) -> &UnboundedSender<ToolJob> {
            self.jobs.as_ref().expect("the lane is still open")
        }

        /// Close the lane and wait for every batch it started to finish.
        async fn drain(&mut self) {
            drop(self.jobs.take());
            let serving = self.serving.take().expect("the lane was serving");
            timeout(serving).await.expect("the lane task ended");
        }

        async fn next_outcome(&mut self) -> ToolOutcome {
            timeout(self.outcomes.recv())
                .await
                .expect("an outcome arrived")
        }

        /// The next `n` outcomes' entity indices, sorted.
        ///
        /// Batches race for a permit as separate tasks, so which of two ready
        /// batches gets in first is not fixed. Nothing depends on that order: a
        /// given agent only ever has one batch in flight.
        async fn next_indices(&mut self, n: usize) -> Vec<u64> {
            let mut seen = Vec::new();
            for _ in 0..n {
                seen.push(self.next_outcome().await.entity.to_bits());
            }
            seen.sort_unstable();
            seen
        }
    }

    /// Bounded so a wedge fails the test instead of hanging it. Generous, since
    /// a passing run never waits.
    async fn timeout<T>(fut: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(30), fut)
            .await
            .expect("the lane made progress")
    }

    /// Entity ids sorted the same way [`Harness::next_indices`] sorts them, so
    /// an expectation does not depend on how bevy packs an id into its bits.
    fn sorted_bits(entities: &[Entity]) -> Vec<u64> {
        let mut bits: Vec<u64> = entities.iter().map(|e| e.to_bits()).collect();
        bits.sort_unstable();
        bits
    }

    fn entity(index: u32) -> Entity {
        Entity::from_raw_u32(index).expect("a small literal index is a valid entity id")
    }

    fn job(index: u32, pairs: Vec<(&'static str, &'static str)>) -> ToolJob {
        job_with(index, pairs, crate::cancel::CancelToken::new())
    }

    fn job_with(
        index: u32,
        pairs: Vec<(&'static str, &'static str)>,
        cancel: crate::cancel::CancelToken,
    ) -> ToolJob {
        ToolJob {
            entity: entity(index),
            exec: Box::new(move || {
                Box::pin(async move {
                    pairs
                        .into_iter()
                        .map(|(a, b)| (a.to_string(), b.to_string()))
                        .collect()
                })
            }),
            cancel,
        }
    }

    /// A job whose batch blocks until `release` fires, signalling `started` once
    /// it is running.
    ///
    /// `notify_one` (not `notify_waiters`) on both signals: it stores a permit
    /// when nobody is waiting yet, so neither side can lose the other's wakeup by
    /// being slow to arm - a real flake on a loaded runner.
    fn held_job(
        index: u32,
        started: Arc<Notify>,
        release: Arc<Notify>,
        cancel: crate::cancel::CancelToken,
    ) -> ToolJob {
        ToolJob {
            entity: entity(index),
            exec: Box::new(move || {
                Box::pin(async move {
                    started.notify_one();
                    release.notified().await;
                    vec![("held".to_string(), "done".to_string())]
                })
            }),
            cancel,
        }
    }

    /// The same, except the wait happens [`off_lane`] - the shape of a
    /// `wait_for_agent` or a tool-approval prompt.
    ///
    /// `started` fires from *inside* the parked future, which `off_lane` only
    /// polls once the permit is already back. Signalling before the call would
    /// race the test against the release.
    fn parking_job(
        index: u32,
        started: Arc<Notify>,
        release: Arc<Notify>,
        cancel: crate::cancel::CancelToken,
    ) -> ToolJob {
        ToolJob {
            entity: entity(index),
            exec: Box::new(move || {
                Box::pin(async move {
                    off_lane(async move {
                        started.notify_one();
                        release.notified().await;
                    })
                    .await;
                    vec![("parked".to_string(), "done".to_string())]
                })
            }),
            cancel,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_lane_runs_batches_and_reports_them() {
        let mut h = Harness::new(1);
        h.submit(job(1, vec![("c", "r")]));
        h.submit(job(2, vec![("c", "r")]));

        let first = h.next_outcome().await;
        assert_eq!(
            first.results,
            vec![("c".to_string(), "r".to_string())],
            "the batch reported its call"
        );
        let mut seen = vec![first.entity.to_bits()];
        seen.extend(h.next_indices(1).await);
        seen.sort_unstable();
        assert_eq!(
            seen,
            sorted_bits(&[entity(1), entity(2)]),
            "both batches were reported"
        );

        h.drain().await;
        assert!(h.outcomes.try_recv().is_err(), "no more outcomes");
    }

    /// The issue #191 regression, at its narrowest.
    ///
    /// A one-wide lane, a batch parked on something only a *later* batch can
    /// deliver. With a fixed worker pool this is a deadlock: the waiter owns the
    /// only worker, so the batch that would release it never runs. Handing the
    /// permit back while parked is what makes both finish.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_parked_batch_lets_the_batch_it_waits_on_run() {
        let mut h = Harness::new(1);
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());

        h.submit(parking_job(
            1,
            started.clone(),
            release.clone(),
            crate::cancel::CancelToken::new(),
        ));
        timeout(started.notified()).await;
        assert_eq!(
            (h.stats.busy(), h.stats.parked()),
            (0, 1),
            "the waiter gave the lane back"
        );

        // Only reachable if the lane is genuinely free. It is what unblocks the
        // waiter, exactly as a child run unblocks its parent.
        let releaser = release.clone();
        h.submit(ToolJob {
            entity: entity(2),
            exec: Box::new(move || {
                Box::pin(async move {
                    releaser.notify_one();
                    vec![("c2".to_string(), "r2".to_string())]
                })
            }),
            cancel: crate::cancel::CancelToken::new(),
        });

        assert_eq!(
            h.next_indices(2).await,
            sorted_bits(&[entity(1), entity(2)]),
            "both batches finished"
        );

        h.drain().await;
    }

    /// The parked batch takes a permit again before it carries on, so the lane's
    /// cap still means something after a wait.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn a_resumed_batch_takes_a_permit_again() {
        let mut h = Harness::new(1);
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        h.submit(parking_job(
            1,
            started.clone(),
            release.clone(),
            crate::cancel::CancelToken::new(),
        ));
        timeout(started.notified()).await;

        // Fill the lane with a batch that will not finish on its own.
        let held_started = Arc::new(Notify::new());
        let held_release = Arc::new(Notify::new());
        h.submit(held_job(
            2,
            held_started.clone(),
            held_release.clone(),
            crate::cancel::CancelToken::new(),
        ));
        timeout(held_started.notified()).await;
        assert_eq!(h.stats.busy(), 1, "the lane is full again");

        // Waking the parked batch is not enough: it has to queue for capacity,
        // and the holder has the only permit. Asserting the absence is what
        // proves the permit was really taken again rather than assumed.
        release.notify_one();
        assert!(
            tokio::time::timeout(Duration::from_millis(250), h.outcomes.recv())
                .await
                .is_err(),
            "the resumed batch waited for a permit instead of running"
        );

        held_release.notify_one();
        let first = h.next_outcome().await;
        assert_eq!(first.entity, entity(2), "the holder finished first");
        let second = h.next_outcome().await;
        assert_eq!(second.entity, entity(1), "then the resumed batch");

        h.drain().await;
        assert_eq!((h.stats.busy(), h.stats.parked()), (0, 0));
    }

    /// Outside a lane task there is no ticket, so `off_lane` is a plain await.
    #[tokio::test]
    async fn off_lane_outside_the_lane_just_awaits() {
        assert_eq!(off_lane(async { 7 }).await, 7);
    }

    /// A cancelled batch is dropped rather than run to completion, and gives its
    /// capacity straight back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_batch_is_abandoned_and_frees_the_lane() {
        let mut h = Harness::new(1);
        let cancel = crate::cancel::CancelToken::new();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        h.submit(held_job(1, started.clone(), release, cancel.clone()));
        timeout(started.notified()).await;
        assert_eq!((h.stats.queued(), h.stats.busy()), (0, 1));

        // Queued behind it, so it can only run once the cancel frees the lane.
        h.submit(job(2, vec![("c2", "r2")]));
        cancel.cancel();

        let next = h.next_outcome().await;
        assert_eq!(next.entity, entity(2), "the queued batch ran");
        h.drain().await;
        assert!(
            h.outcomes.try_recv().is_err(),
            "the cancelled batch reported nothing"
        );
        assert_eq!(h.stats.busy(), 0, "and gave its permit back");
    }

    /// Cancelling a batch that is parked on a wait leaves the counters straight:
    /// it was never holding a permit, so nothing is handed back twice.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_parked_batch_leaves_the_counters_straight() {
        let mut h = Harness::new(1);
        let cancel = crate::cancel::CancelToken::new();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        h.submit(parking_job(1, started.clone(), release, cancel.clone()));
        timeout(started.notified()).await;
        assert_eq!((h.stats.busy(), h.stats.parked()), (0, 1));

        cancel.cancel();
        h.submit(job(2, vec![("c2", "r2")]));
        let next = h.next_outcome().await;
        assert_eq!(next.entity, entity(2));

        h.drain().await;
        assert_eq!((h.stats.busy(), h.stats.parked()), (0, 0));
    }

    /// A cancel that lands while the batch is still waiting for capacity drops it
    /// without it ever running, and takes it off the queue count.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_batch_cancelled_while_queued_never_runs() {
        let mut h = Harness::new(1);
        let blocker = crate::cancel::CancelToken::new();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        h.submit(held_job(1, started.clone(), release.clone(), blocker));
        timeout(started.notified()).await;

        let cancel = crate::cancel::CancelToken::new();
        h.submit(job_with(2, vec![("c2", "r2")], cancel.clone()));
        cancel.cancel();
        release.notify_one();

        let first = h.next_outcome().await;
        assert_eq!(first.entity, entity(1));
        h.drain().await;
        assert!(
            h.outcomes.try_recv().is_err(),
            "the cancelled batch never produced results"
        );
        assert_eq!(h.stats.queued(), 0, "and left the queue count clean");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_lane_runs_batches_concurrently_up_to_its_cap() {
        let h = Harness::new(3);
        // Three jobs that each block on a rendezvous: they can only all finish if
        // they run concurrently. `tokio::sync::Barrier` rather than a hand-rolled
        // counter + Notify: `notify_waiters` only wakes ALREADY-registered
        // waiters, so a counter check has a lost-wakeup window between loading
        // the count and registering on `notified()`.
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        for i in 1..=3u32 {
            let barrier = barrier.clone();
            h.submit(ToolJob {
                entity: entity(i),
                exec: Box::new(move || {
                    Box::pin(async move {
                        barrier.wait().await;
                        vec![("c".to_string(), "r".to_string())]
                    })
                }),
                cancel: crate::cancel::CancelToken::new(),
            });
        }
        let mut h = h;
        h.drain().await;
        for _ in 0..3 {
            timeout(h.outcomes.recv()).await.expect("outcome present");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_lane_survives_a_dropped_outcome_receiver() {
        let mut h = Harness::new(1);
        h.submit(job(9, vec![("c", "r")]));
        // Nobody to receive the outcome: the lane must still drain the job and
        // not panic on the failed send.
        h.outcomes.close();
        h.drain().await;
    }

    /// Relief widens the lane so batches queued behind a wedge can run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn relief_widens_the_lane() {
        let mut h = Harness::new(1);
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        h.submit(held_job(
            1,
            started.clone(),
            release.clone(),
            crate::cancel::CancelToken::new(),
        ));
        timeout(started.notified()).await;
        h.submit(job(2, vec![("c2", "r2")]));
        assert!(h.stats.is_saturated(), "full, with a batch behind it");

        assert_eq!(h.lane.relieve(0), 0, "relieving nothing changes nothing");
        assert_eq!(h.lane.relieve(1), 1);
        assert_eq!(h.stats.workers(), 2, "the cap moved with the permits");

        let freed = h.next_outcome().await;
        assert_eq!(freed.entity, entity(2), "the queued batch got in");

        release.notify_one();
        let held = h.next_outcome().await;
        assert_eq!(held.entity, entity(1));
        h.drain().await;
    }

    /// A lane with no capacity is still reported as one wide, matching
    /// [`ToolLane::new`]'s own clamp - otherwise the saturation check compares
    /// against a width that never existed.
    #[tokio::test]
    async fn a_zero_width_lane_is_clamped_to_one() {
        assert_eq!(ToolLaneStats::new(0).workers(), 1);
        let mut h = Harness::new(0);
        h.submit(job(7, vec![("c", "r")]));
        assert_eq!(h.next_outcome().await.entity, entity(7));
        h.drain().await;
    }

    #[test]
    fn lane_stats_track_queue_depth_and_saturation() {
        let stats = ToolLaneStats::new(2);
        assert_eq!((stats.queued(), stats.busy(), stats.parked()), (0, 0, 0));
        assert!(!stats.is_saturated(), "an idle lane is not saturated");

        stats.enqueued();
        stats.enqueued();
        stats.enqueued();
        assert_eq!(stats.queued(), 3);
        // Two batches take permits: two leave the queue, two occupy the lane.
        stats.started();
        stats.started();
        assert_eq!((stats.queued(), stats.busy()), (1, 2));
        assert!(
            stats.is_saturated(),
            "the lane is full with a batch still queued"
        );

        // One steps off to wait: it stops occupying the lane.
        stats.began_park();
        assert_eq!((stats.busy(), stats.parked()), (1, 1));
        assert!(!stats.is_saturated(), "parked capacity is capacity");
        stats.resumed();
        assert_eq!((stats.busy(), stats.parked()), (2, 0));

        stats.began_park();
        stats.ended_park();
        assert_eq!((stats.busy(), stats.parked()), (1, 0));

        stats.finished();
        stats.abandoned();
        assert_eq!((stats.queued(), stats.busy()), (0, 0));
    }
}
