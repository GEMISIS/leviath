//! The live side of the schema.
//!
//! `/ws` hands every frame to every listener and leaves the sorting to them. A
//! subscription here says which frames it wants and which runs it is about,
//! and both filters are applied before a frame is converted: a console
//! watching one run of five thousand pays for one run's frames rather than for
//! the fleet's.
//!
//! Every subscription opens with a `SubscriptionOpenedEvent`, before anything
//! the daemon sent. That frame is what tells a client the stream is live,
//! which process is numbering it, and whether the daemon behind it is
//! reachable: the counterpart of the greeting `/ws` sends.
//!
//! The daemon is never slowed down by a listener. The broadcast never waits
//! for a receiver, and a subscription that cannot keep up is handed an
//! `EventsDroppedEvent` saying how many it missed and left open, rather than
//! quietly losing frames or holding up the rest.
//!
//! A client that was away comes back with `since`: the bus keeps its last few
//! thousand frames, and a resumed subscription replays the ones above that
//! number before it goes live.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::sync::Arc;

use async_graphql::{Context, ID, Subscription};
use futures_util::{Stream, StreamExt};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use super::super::core::error::ServeError;
use super::super::core::runs::predicate::{MatchContext, RunPredicate, RunTree};
use super::super::events::{ServerEvent, Stamped};
use super::super::types::AppState;
use super::error::IntoGraphql;
use super::events::{
    MachineEventFrame, MachineEventType, RunEventFrame, RunEventType, SubscriptionOpenedEvent,
    UpdateJobEventFrame,
};
use super::filter::run_predicate;
use super::paging::walk::Verdict;
use super::scalars::BigInt;
use super::types::run::RunFilter;

/// Which runs a subscription is about.
///
/// Built once, when the subscription starts, and then consulted per frame. It
/// only ever grows: a run that starts matching joins, and a run that stops
/// matching stays, because losing the frame that says a run finished is worse
/// than one extra row.
enum Scope {
    /// Every run, which is what no filter means.
    Everything,
    /// The runs a filter named, and the filter itself, so a run that was not
    /// one of them can still join later.
    Named(Named),
}

/// The runs a filter named, and what decides whether another one joins.
struct Named {
    /// The runs in scope right now.
    runs: HashSet<String>,
    /// The compiled filter, consulted for a run that is not in `runs` yet.
    predicate: Arc<dyn RunPredicate>,
    /// Whether a run spawned by a run in scope joins the scope.
    include_descendants: bool,
    /// Whether a status frame could change what this filter answers.
    ///
    /// Decided once, when the subscription starts, because the filter does
    /// not change afterwards. A filter it is false for is never asked again
    /// about a run the index has already answered for, which is what keeps a
    /// fleet's heartbeat from costing every subscriber a reading of the whole
    /// run store per frame.
    status_can_widen: bool,
    /// Runs whose spawn frame arrived before the run index had their record.
    ///
    /// Asked again on each of their frames, whatever the filter names, until
    /// the index answers. Their frames wait here meanwhile, so a run that
    /// joins arrives whole, its spawn frame first.
    pending: HashMap<String, Held>,
}

/// How many frames of a pending run are looked at before giving up on it.
///
/// The index lags a spawn frame by moments, not by dozens of frames. A run it
/// still cannot answer for after this many is a run whose record is not
/// coming, and asking again on every frame would cost a reading of the run
/// store per frame for nothing.
const HOLD_LIMIT: usize = 32;

/// The frames of a pending run, waiting for the index to answer.
#[derive(Default)]
struct Held {
    /// The frames the subscription asked for, oldest first.
    frames: Vec<Stamped>,
    /// Every frame of the run seen while pending, kept or not.
    seen: usize,
}

/// The filter fields a run's own status frame can change the answer to.
///
/// A status frame carries the new status, the reason it is waiting, the title
/// the titling pass landed, and the two clocks that move with them. Nothing
/// else on a run's record moves without a frame of its own.
const MUTABLE_FIELDS: [&str; 5] = [
    "status:",
    "waitReason:",
    "title:",
    "updatedAt:",
    "lastProgressAt:",
];

/// Whether a status frame could change what this filter answers.
///
/// Read off the canonical rendering the cursor digest is taken over, so a
/// filter has one description here rather than two. A field name that turns up
/// inside a string somebody searched for reads as a mention, which costs a
/// re-check that answers the same way - never a missed one.
fn mentions_mutable_fields(rendered: &str) -> bool {
    MUTABLE_FIELDS.iter().any(|field| rendered.contains(field))
}

/// The runs a filter names right now, as a scope that can re-check one later.
///
/// This is the seam the run listing and a subscription share: a filter
/// compiles into a predicate, and the predicate is what both a page and a
/// stream consult. A filter with nothing set compiles to no predicate at all,
/// which is [`Scope::Everything`].
async fn matching(
    state: &AppState,
    filter: RunFilter,
    include_descendants: bool,
) -> Result<Scope, ServeError> {
    // The filter is consulted twice, once as a whole walk and then per frame,
    // so the compile below takes a copy and the walk takes the original.
    let Some(predicate) = run_predicate::compile(filter.clone())? else {
        return Ok(Scope::Everything);
    };
    // Read off the predicate's own digest part, which is the canonical
    // rendering of what the client wrote, rather than rendering it again.
    let status_can_widen = mentions_mutable_fields(&predicate.digest_part());
    // The full walk, reads included, so the first frame a subscriber sees is
    // about a run a page of the same filter would have listed.
    let runs = run_predicate::selection(Some(filter), state)
        .await?
        .into_iter()
        .map(|meta| meta.run_id.clone())
        .collect();
    Ok(Scope::Named(Named {
        runs,
        predicate,
        include_descendants,
        status_can_widen,
        pending: HashMap::new(),
    }))
}

/// Whether one run satisfies the filter as things stand, or `None` when the
/// run index has no record of it yet.
///
/// Answered from the cached run index and the half of the filter that reads
/// nothing: a frame is not the moment to open a run's files, so a verdict that
/// would need them counts as not matching. A run the index has not seen yet,
/// which a spawn frame can outrun, has no answer at all.
async fn index_verdict(
    state: &AppState,
    predicate: &Arc<dyn RunPredicate>,
    run_id: &str,
) -> Option<bool> {
    let snapshot = state.caches.run_index.snapshot().await;
    let meta = snapshot.get(run_id).cloned()?;
    let runs = snapshot.into_runs();
    let ctx = MatchContext {
        now: leviath_core::duration::now_secs(),
        tree: Arc::new(RunTree::of(&runs)),
    };
    Some(predicate.verdict(&meta, &ctx) == Verdict::Keep)
}

/// Queue one frame for delivery, if it was asked for and has a run shape.
fn deliver(stamped: Stamped, asked_for: bool, ready: &mut VecDeque<RunEventFrame>) {
    if asked_for && let Some(frame) = super::events::run_frame(stamped) {
        ready.push_back(frame);
    }
}

impl Scope {
    /// Queue this frame if its run is in scope, widening the scope where the
    /// frame is what widens it.
    ///
    /// Consulted for every frame, asked for or not: a spawn frame the client
    /// filtered out by type still brings its run into scope.
    async fn admit(
        &mut self,
        state: &AppState,
        stamped: Stamped,
        asked_for: bool,
        ready: &mut VecDeque<RunEventFrame>,
    ) {
        match self {
            Scope::Everything => deliver(stamped, asked_for, ready),
            Scope::Named(named) => named.admit(state, stamped, asked_for, ready).await,
        }
    }
}

impl Named {
    /// Queue, hold or drop one frame.
    async fn admit(
        &mut self,
        state: &AppState,
        stamped: Stamped,
        asked_for: bool,
        ready: &mut VecDeque<RunEventFrame>,
    ) {
        let run_id = stamped.event.run_id().to_string();
        // A machine-level frame reaches a run scope only as the link or
        // lost-events frame: those explain why a run's frames stopped or may
        // be missing, so a scoped subscription still gets them, and
        // `run_frames` drops the others before any scope is consulted because
        // they are about the install and belong to `machineEvents`.
        if run_id.is_empty() {
            return deliver(stamped, asked_for, ready);
        }
        if !self.runs.contains(&run_id) {
            self.widen(state, &stamped.event, &run_id, ready).await;
        }
        if self.runs.contains(&run_id) {
            deliver(stamped, asked_for, ready);
        } else if let Some(held) = self.pending.get_mut(&run_id) {
            held.seen += 1;
            if held.seen > HOLD_LIMIT {
                self.pending.remove(&run_id);
            } else if asked_for {
                held.frames.push(stamped);
            }
        }
    }

    /// Bring a run that is not in scope into it, when this frame is what
    /// says it now belongs.
    async fn widen(
        &mut self,
        state: &AppState,
        event: &ServerEvent,
        run_id: &str,
        ready: &mut VecDeque<RunEventFrame>,
    ) {
        let ask = match event {
            // A spawn is what widens the scope: by its parent when the client
            // asked for descendants, and by the filter itself otherwise. It is
            // pending until the index answers, which is usually at once.
            ServerEvent::AgentSpawned { parent_id, .. } => {
                if self.include_descendants
                    && parent_id
                        .as_ref()
                        .is_some_and(|parent| self.runs.contains(parent))
                {
                    self.runs.insert(run_id.to_string());
                    return;
                }
                self.pending.insert(run_id.to_string(), Held::default());
                true
            }
            // A status change is the other moment a run's answer can have
            // changed, and it is only asked of a filter one could change.
            ServerEvent::AgentStatus { .. } => {
                self.status_can_widen || self.pending.contains_key(run_id)
            }
            _ => self.pending.contains_key(run_id),
        };
        if !ask {
            return;
        }
        match index_verdict(state, &self.predicate, run_id).await {
            Some(true) => {
                self.runs.insert(run_id.to_string());
                let held = self.pending.remove(run_id).unwrap_or_default();
                for frame in held.frames {
                    deliver(frame, true, ready);
                }
            }
            Some(false) => {
                self.pending.remove(run_id);
            }
            None => {}
        }
    }
}

/// The frame types a subscription asked for.
///
/// A set rather than a list: the check runs once per frame per subscriber, and
/// an empty set means everything.
struct Types<T>(HashSet<T>);

impl<T: Eq + Hash> Types<T> {
    /// Whether a frame of this type was asked for.
    fn wants(&self, kind: T) -> bool {
        self.0.is_empty() || self.0.contains(&kind)
    }
}

/// Where a subscription's frames come from: the kept frames it replays, then
/// the bus.
struct Feed {
    /// The kept frames still to replay, oldest first.
    backlog: VecDeque<Stamped>,
    /// The live frames, registered before the backlog was read.
    frames: BroadcastStream<Stamped>,
}

impl Feed {
    /// The next frame, or how many went past unread, or `None` once the bus
    /// has gone.
    async fn next(&mut self) -> Option<Result<Stamped, u64>> {
        if let Some(kept) = self.backlog.pop_front() {
            return Some(Ok(kept));
        }
        let next = self.frames.next().await?;
        Some(next.map_err(|BroadcastStreamRecvError::Lagged(missed)| missed))
    }
}

/// Where one subscription starts: what it reads from, the number its greeting
/// carries, and the gap it opens with.
struct Start {
    /// The kept frames to replay, then the live ones.
    feed: Feed,
    /// The number the greeting carries.
    seq: u64,
    /// How many frames this subscription cannot be handed, when it resumed
    /// past what the bus still keeps.
    gap: Option<u64>,
}

/// Register a subscription on the bus, resuming after `since` when asked to.
///
/// A plain subscription replays nothing and opens on the bus's latest number.
/// One that names another server's instance cannot resume: its numbers mean
/// nothing here, so it opens with a gap as large as everything this process
/// has sent. One that resumes on this server replays every kept frame above
/// `since`, and opens with a gap when some of those are no longer kept.
fn start(state: &AppState, since: Option<BigInt>, instance: Option<ID>) -> Start {
    let elsewhere =
        instance.is_some_and(|id| id.as_str() != super::super::events::server_instance());
    let since = since.map(|since| u64::try_from(since.0).unwrap_or_default());
    let resume_from = match (elsewhere, since) {
        (false, Some(since)) => since,
        _ => u64::MAX,
    };
    let resume = state.event_tx.resume(resume_from);
    let (seq, gap) = match (elsewhere, since) {
        (true, _) => (resume.at, Some(resume.at)),
        (false, Some(since)) => (
            since.min(resume.at),
            (resume.evicted > 0).then_some(resume.evicted),
        ),
        (false, None) => (resume.at, None),
    };
    Start {
        feed: Feed {
            backlog: resume.kept,
            frames: BroadcastStream::new(resume.frames),
        },
        seq,
        gap,
    }
}

/// Everything one run subscription carries between frames.
struct RunLive {
    /// The frames, kept then live.
    feed: Feed,
    /// Frames ready to hand over, oldest first.
    ready: VecDeque<RunEventFrame>,
    /// Which frame types this subscription asked for.
    types: Types<RunEventType>,
    /// Which runs it is about.
    scope: Scope,
    /// What a filter re-check reads.
    state: AppState,
    /// The last sequence number that reached this subscription, so a gap is
    /// reported against something.
    last_seq: u64,
}

/// Everything one machine subscription carries between frames.
struct MachineLive {
    /// The frames, kept then live.
    feed: Feed,
    /// A gap to report before anything else.
    gap: Option<u64>,
    /// Which frame types this subscription asked for.
    types: Types<MachineEventType>,
    /// The last sequence number that reached this subscription.
    last_seq: u64,
}

/// Everything one update-job subscription carries between frames.
struct JobLive {
    /// The frames, as they come off the bus.
    feed: Feed,
    /// The job this subscription is about.
    job: String,
    /// The last sequence number that reached this subscription.
    last_seq: u64,
}

/// The opening frame first, then whatever the daemon sends.
fn opening<Frame>(opened: Frame, rest: impl Stream<Item = Frame>) -> impl Stream<Item = Frame> {
    futures_util::stream::once(std::future::ready(opened)).chain(rest)
}

/// The run frames this subscription wants, one at a time.
fn run_frames(live: RunLive) -> impl Stream<Item = RunEventFrame> + use<> {
    futures_util::stream::unfold(live, |mut live| async move {
        loop {
            if let Some(frame) = live.ready.pop_front() {
                return Some((frame, live));
            }
            let stamped = match live.feed.next().await? {
                Ok(stamped) => stamped,
                Err(missed) => {
                    let gap = super::events::dropped(missed, live.last_seq);
                    live.ready.push_back(RunEventFrame::EventsDropped(gap));
                    continue;
                }
            };
            live.last_seq = stamped.seq;
            let asked_for = match RunEventType::of(&stamped.event) {
                Some(kind) => live.types.wants(kind),
                // The link and lost-events frames are not in the run
                // vocabulary and are never filtered out: they explain a
                // silence.
                None => matches!(
                    stamped.event,
                    ServerEvent::DaemonLink { .. } | ServerEvent::EventsDropped { .. }
                ),
            };
            live.scope
                .admit(&live.state, stamped, asked_for, &mut live.ready)
                .await;
        }
    })
}

/// The machine frames this subscription wants, one at a time.
fn machine_frames(live: MachineLive) -> impl Stream<Item = MachineEventFrame> + use<> {
    futures_util::stream::unfold(live, |mut live| async move {
        if let Some(missed) = live.gap.take() {
            let gap = super::events::dropped(missed, live.last_seq);
            return Some((MachineEventFrame::EventsDropped(gap), live));
        }
        loop {
            match live.feed.next().await? {
                Ok(stamped) => {
                    live.last_seq = stamped.seq;
                    // Lost events are never filtered out, like this
                    // subscription's own `EventsDropped`.
                    let asked_for = match MachineEventType::of(&stamped.event) {
                        Some(kind) => live.types.wants(kind),
                        None => matches!(stamped.event, ServerEvent::EventsDropped { .. }),
                    };
                    if asked_for && let Some(frame) = super::events::machine_frame(stamped) {
                        return Some((frame, live));
                    }
                }
                Err(missed) => {
                    let gap = super::events::dropped(missed, live.last_seq);
                    return Some((MachineEventFrame::EventsDropped(gap), live));
                }
            }
        }
    })
}

/// One job's frames, one at a time.
fn job_frames(live: JobLive) -> impl Stream<Item = UpdateJobEventFrame> + use<> {
    futures_util::stream::unfold(live, |mut live| async move {
        loop {
            match live.feed.next().await? {
                Ok(stamped) => {
                    live.last_seq = stamped.seq;
                    if let Some(frame) = super::events::update_job_frame(stamped, &live.job) {
                        return Some((frame, live));
                    }
                }
                Err(missed) => {
                    let gap = super::events::dropped(missed, live.last_seq);
                    return Some((UpdateJobEventFrame::EventsDropped(gap), live));
                }
            }
        }
    })
}

/// The sequence number the opening frame reports, which is where this
/// subscription's own numbering starts.
fn opened_seq(opened: &SubscriptionOpenedEvent) -> u64 {
    u64::try_from(opened.seq.0).unwrap_or_default()
}

/// The resolver state behind the `Subscription` type.
pub(crate) struct Subscription_;

/// The live side: what the fleet is doing as it does it, over a WebSocket at
/// `/ws/graphql`.
///
/// Three streams rather than one, because they answer three different
/// questions: what runs are doing, what the machine is doing, and how one
/// update is going. Each opens with a `SubscriptionOpenedEvent` and then
/// carries only the frames it is about.
///
/// Delivery is at-most-once. A client that was away resumes with `since` and
/// the `serverInstance` it was numbered by, and is handed what the server
/// still keeps; anything older is announced as a gap, and the run itself is
/// how a client reconstructs that.
#[Subscription(name = "Subscription")]
impl Subscription_ {
    /// What runs are doing, as they do it.
    ///
    /// `filter` is the run listing's own filter, so "every failed run of this
    /// blueprint" is the same words here as in `runs`. It is resolved to a set
    /// of runs when the subscription starts. After that the set only grows: a
    /// run that spawns is checked against the filter once its record exists,
    /// and `includeDescendants` puts the sub-agents of a run in scope as they
    /// spawn. A run that stops matching keeps sending, because losing the
    /// frame that says a run finished is worse than one extra row.
    ///
    /// A run outside the set is checked again on each of its status changes
    /// where the filter names something a status change can alter - `status`,
    /// `waitReason`, `title`, `updatedAt` or `lastProgressAt`. A filter that
    /// names none of them cannot start matching a run because of a status
    /// frame, so nothing is re-read.
    ///
    /// Only the filter's in-memory half decides those later checks. A
    /// condition that would have to open a file reads as "not matching" for
    /// now, and is asked again on that run's next status change.
    ///
    /// A run whose spawn frame arrives before the server has its record is
    /// asked again on each of its frames, whatever the filter names, and its
    /// frames are held meanwhile. Once the record is there the run joins with
    /// those frames, spawn first, or is dropped with them.
    ///
    /// `since` resumes a stream: the frames this server still keeps that are
    /// numbered above it arrive first, in order, and then the live ones, with
    /// nothing missed or repeated between. Pass the `serverInstance` that
    /// numbered `since`. When it names another process, or `since` is older
    /// than what is kept, an `EventsDroppedEvent` follows the opening frame:
    /// re-read what you show.
    ///
    /// Filter fields that describe a listing rather than a run - `query`,
    /// `sort` and their neighbours - have nothing to describe on a stream and
    /// are ignored.
    ///
    /// `DaemonLinkChangedEvent` arrives whatever `types` says, and whatever
    /// the scope is: a run's frames stopping because the daemon went away
    /// looks exactly like a quiet run without it.
    async fn run_events(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which runs to watch. Omitted means every run.")] filter: Option<
            RunFilter,
        >,
        #[graphql(desc = "Only these frame types; omitted or empty means all of them.")]
        types: Option<Vec<RunEventType>>,
        #[graphql(
            desc = "Include the sub-agents of the runs in scope, as they spawn.",
            default = false
        )]
        include_descendants: bool,
        #[graphql(desc = "Resume after this frame number: replay the kept frames above it first.")]
        since: Option<BigInt>,
        #[graphql(desc = "The `serverInstance` that numbered `since`.")] server_instance: Option<
            ID,
        >,
    ) -> async_graphql::Result<impl Stream<Item = RunEventFrame> + use<>> {
        let state = ctx.data_unchecked::<AppState>();
        // The receiver is registered before anything is awaited: a frame sent
        // while the filter is still being resolved is then buffered rather
        // than lost, and numbered above the greeting rather than under it.
        let Start { feed, seq, gap } = start(state, since, server_instance);
        let scope = match filter {
            Some(filter) => matching(state, filter, include_descendants).await.gql()?,
            None => Scope::Everything,
        };
        let opened = super::events::opened(state, seq);
        let ready = gap
            .map(|missed| RunEventFrame::EventsDropped(super::events::dropped(missed, seq)))
            .into_iter()
            .collect();
        let live = RunLive {
            feed,
            ready,
            types: Types(types.into_iter().flatten().collect()),
            scope,
            state: state.clone(),
            last_seq: opened_seq(&opened),
        };
        Ok(opening(
            RunEventFrame::SubscriptionOpened(opened),
            run_frames(live),
        ))
    }

    /// What is happening to the machine this server runs on.
    ///
    /// The frames that are about no run: the link to the daemon, the health of
    /// the config file, and the progress of a self-update. `/ws` mixes these
    /// in with everything else; here they are their own stream, so a settings
    /// screen subscribes to them without seeing a fleet's worth of log lines.
    ///
    /// `since` and `serverInstance` resume it the way they resume
    /// `runEvents`.
    async fn machine_events(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Only these frame types; omitted or empty means all of them.")]
        types: Option<Vec<MachineEventType>>,
        #[graphql(desc = "Resume after this frame number: replay the kept frames above it first.")]
        since: Option<BigInt>,
        #[graphql(desc = "The `serverInstance` that numbered `since`.")] server_instance: Option<
            ID,
        >,
    ) -> impl Stream<Item = MachineEventFrame> + use<> {
        let state = ctx.data_unchecked::<AppState>();
        let Start { feed, seq, gap } = start(state, since, server_instance);
        let opened = super::events::opened(state, seq);
        let live = MachineLive {
            feed,
            gap,
            types: Types(types.into_iter().flatten().collect()),
            last_seq: opened_seq(&opened),
        };
        opening(
            MachineEventFrame::SubscriptionOpened(opened),
            machine_frames(live),
        )
    }

    /// How one update started by `startUpdate` is going.
    ///
    /// Narrowed to that job, because a console watching an install is watching
    /// one install. The frame that says the job finished is the last one it
    /// produces; the stream itself stays open until the client closes it.
    async fn update_job_events(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The job, as `startUpdate` answered with.")] id: ID,
    ) -> impl Stream<Item = UpdateJobEventFrame> + use<> {
        let state = ctx.data_unchecked::<AppState>();
        let Start { feed, seq, .. } = start(state, None, None);
        let opened = super::events::opened(state, seq);
        let live = JobLive {
            feed,
            job: id.to_string(),
            last_seq: opened_seq(&opened),
        };
        opening(
            UpdateJobEventFrame::SubscriptionOpened(opened),
            job_frames(live),
        )
    }
}

#[cfg(test)]
#[path = "subscription_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "subscription_replay_tests.rs"]
mod replay_tests;
