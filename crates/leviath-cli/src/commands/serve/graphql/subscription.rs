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

use std::collections::HashSet;
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
    Named {
        /// The runs in scope right now.
        runs: HashSet<String>,
        /// The compiled filter, consulted for a run that is not in `runs` yet.
        predicate: Arc<dyn RunPredicate>,
        /// Whether a run spawned by a run in scope joins the scope.
        include_descendants: bool,
    },
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
    // The full walk, reads included, so the first frame a subscriber sees is
    // about a run a page of the same filter would have listed.
    let runs = run_predicate::selection(Some(filter), state)
        .await?
        .into_iter()
        .map(|meta| meta.run_id.clone())
        .collect();
    Ok(Scope::Named {
        runs,
        predicate,
        include_descendants,
    })
}

/// Whether one run satisfies the filter as things stand.
///
/// Answered from the cached run index and the half of the filter that reads
/// nothing: a frame is not the moment to open a run's files, so a verdict that
/// would need them counts as not matching until a later frame asks again. A
/// run the index has not seen yet, which a spawn frame can outrun, does not
/// match either.
async fn still_matches(state: &AppState, predicate: &Arc<dyn RunPredicate>, run_id: &str) -> bool {
    let snapshot = state.caches.run_index.snapshot().await;
    let Some(meta) = snapshot.get(run_id).cloned() else {
        return false;
    };
    let runs = snapshot.into_runs();
    let ctx = MatchContext {
        now: leviath_core::duration::now_secs(),
        tree: Arc::new(RunTree::of(&runs)),
    };
    predicate.verdict(&meta, &ctx) == Verdict::Keep
}

impl Scope {
    /// Whether this frame's run is in scope, widening the scope where the
    /// frame is what widens it.
    async fn keeps(&mut self, state: &AppState, event: &ServerEvent) -> bool {
        let Scope::Named {
            runs,
            predicate,
            include_descendants,
        } = self
        else {
            return true;
        };
        match event {
            // A spawn is what widens the scope, so it is considered before the
            // membership test that would otherwise reject the new run: by its
            // parent when the client asked for descendants, and by the filter
            // itself once the daemon has written the run's record.
            ServerEvent::AgentSpawned {
                run_id, parent_id, ..
            } => {
                let inherited = *include_descendants
                    && parent_id
                        .as_ref()
                        .is_some_and(|parent| runs.contains(parent));
                if inherited || still_matches(state, predicate, run_id).await {
                    runs.insert(run_id.clone());
                }
            }
            // A status change is the other moment a run's answer can have
            // changed, and it is the cheap one: the record it reads is already
            // in the index.
            ServerEvent::AgentStatus { run_id, .. }
                if !runs.contains(run_id) && still_matches(state, predicate, run_id).await =>
            {
                runs.insert(run_id.clone());
            }
            _ => {}
        }
        match event.run_id() {
            // A machine-level frame, which reaches a run scope only as the
            // link frame: that one explains why a run's frames stopped, so a
            // scoped subscription still gets it, and `run_frames` drops the
            // others before any scope is consulted because they are about the
            // install and belong to `machineEvents`.
            "" => true,
            run_id => runs.contains(run_id),
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

/// Everything one run subscription carries between frames.
struct RunLive {
    /// The frames, as they come off the bus.
    frames: BroadcastStream<Stamped>,
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
    /// The frames, as they come off the bus.
    frames: BroadcastStream<Stamped>,
    /// Which frame types this subscription asked for.
    types: Types<MachineEventType>,
    /// The last sequence number that reached this subscription.
    last_seq: u64,
}

/// Everything one update-job subscription carries between frames.
struct JobLive {
    /// The frames, as they come off the bus.
    frames: BroadcastStream<Stamped>,
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
            match live.frames.next().await? {
                Ok(stamped) => {
                    live.last_seq = stamped.seq;
                    let asked_for = match RunEventType::of(&stamped.event) {
                        Some(kind) => live.types.wants(kind),
                        // The link frame is not in the run vocabulary and is
                        // never filtered out: it is what explains a silence.
                        None => matches!(stamped.event, ServerEvent::DaemonLink { .. }),
                    };
                    if asked_for
                        && live.scope.keeps(&live.state, &stamped.event).await
                        && let Some(frame) = super::events::run_frame(stamped)
                    {
                        return Some((frame, live));
                    }
                }
                Err(BroadcastStreamRecvError::Lagged(missed)) => {
                    let gap = super::events::dropped(missed, live.last_seq);
                    return Some((RunEventFrame::EventsDropped(gap), live));
                }
            }
        }
    })
}

/// The machine frames this subscription wants, one at a time.
fn machine_frames(live: MachineLive) -> impl Stream<Item = MachineEventFrame> + use<> {
    futures_util::stream::unfold(live, |mut live| async move {
        loop {
            match live.frames.next().await? {
                Ok(stamped) => {
                    live.last_seq = stamped.seq;
                    if MachineEventType::of(&stamped.event)
                        .is_some_and(|kind| live.types.wants(kind))
                        && let Some(frame) = super::events::machine_frame(stamped)
                    {
                        return Some((frame, live));
                    }
                }
                Err(BroadcastStreamRecvError::Lagged(missed)) => {
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
            match live.frames.next().await? {
                Ok(stamped) => {
                    live.last_seq = stamped.seq;
                    if let Some(frame) = super::events::update_job_frame(stamped, &live.job) {
                        return Some((frame, live));
                    }
                }
                Err(BroadcastStreamRecvError::Lagged(missed)) => {
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
/// Delivery is at-most-once, so a stream is how a client stays current and not
/// how it reconstructs the past: for that, read the run.
#[Subscription(name = "Subscription")]
impl Subscription_ {
    /// What runs are doing, as they do it.
    ///
    /// `filter` is the run listing's own filter, so "every failed run of this
    /// blueprint" is the same words here as in `runs`. It is resolved to a set
    /// of runs when the subscription starts. After that the set only grows: a
    /// run that spawns is checked against the filter once its record exists, a
    /// run outside the set is checked again on each of its status changes, and
    /// `includeDescendants` puts the sub-agents of a run in scope as they
    /// spawn. A run that stops matching keeps sending, because losing the
    /// frame that says a run finished is worse than one extra row.
    ///
    /// Only the filter's in-memory half decides those later checks. A
    /// condition that would have to open a file reads as "not matching" for
    /// now, and is asked again on that run's next frame.
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
    ) -> async_graphql::Result<impl Stream<Item = RunEventFrame> + use<>> {
        let state = ctx.data_unchecked::<AppState>();
        // Subscribed before anything is awaited, so a frame sent while the
        // filter is still being resolved is buffered rather than lost.
        let frames = BroadcastStream::new(state.event_tx.subscribe());
        let scope = match filter {
            Some(filter) => matching(state, filter, include_descendants).await.gql()?,
            None => Scope::Everything,
        };
        let opened = super::events::opened(state);
        let live = RunLive {
            frames,
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
    async fn machine_events(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Only these frame types; omitted or empty means all of them.")]
        types: Option<Vec<MachineEventType>>,
    ) -> impl Stream<Item = MachineEventFrame> + use<> {
        let state = ctx.data_unchecked::<AppState>();
        let frames = BroadcastStream::new(state.event_tx.subscribe());
        let opened = super::events::opened(state);
        let live = MachineLive {
            frames,
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
        let frames = BroadcastStream::new(state.event_tx.subscribe());
        let opened = super::events::opened(state);
        let live = JobLive {
            frames,
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
