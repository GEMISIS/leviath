//! The live side of the schema.
//!
//! `/ws` hands every frame to every listener and leaves the sorting to them. A
//! subscription here says which frame types it wants and which runs it is
//! about, and both filters are applied before a frame is converted: a console
//! watching one run of five thousand pays for one run's frames rather than for
//! the fleet's.
//!
//! The daemon is never slowed down by a listener. The broadcast never waits for
//! a receiver, and a subscription that cannot keep up is told it fell behind
//! (`EventsDropped`) instead of quietly missing frames or holding up the rest.

use std::collections::HashSet;

use async_graphql::{Context, Subscription};
use futures_util::{Stream, StreamExt};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;

use super::super::events::ServerEvent;
use super::super::types::AppState;
use super::events::{EventsDropped, RunEvent, RunEventType};
use super::scalars::BigInt;

/// Which runs a subscription is about.
///
/// Built once, when the subscription starts, and then consulted per frame. A
/// scope that grows (`includeDescendants`) grows here as the spawns arrive, so
/// a fan-out started after subscribing is covered without the client
/// re-subscribing.
struct Scope {
    /// The runs named outright. Empty means every run.
    runs: HashSet<String>,
    /// Whether a run spawned by a run in scope joins the scope.
    include_descendants: bool,
}

impl Scope {
    /// Whether this frame's run is in scope.
    ///
    /// The machine-level frames carry no run, and the run-level ones are kept
    /// only for the runs asked for.
    fn keeps(&mut self, event: &ServerEvent) -> bool {
        if self.runs.is_empty() {
            return true;
        }
        // A spawn is what widens the scope, so it is considered before the
        // membership test that would otherwise reject the new run.
        if self.include_descendants
            && let ServerEvent::AgentSpawned {
                run_id, parent_id, ..
            } = event
            && parent_id
                .as_ref()
                .is_some_and(|parent| self.runs.contains(parent))
        {
            self.runs.insert(run_id.clone());
        }
        match event.run_id() {
            // A machine-level frame. `daemon_link` explains why a run's frames
            // stopped, so a scoped subscription still gets it; the others are
            // about the install and go to unscoped subscriptions only, exactly
            // as on `/ws`.
            "" => matches!(event, ServerEvent::DaemonLink { .. }),
            run_id => self.runs.contains(run_id),
        }
    }
}

/// The frame types a subscription wants.
///
/// A set rather than a list: the check runs once per frame per subscriber, and
/// an empty set means everything.
struct Types(HashSet<RunEventType>);

impl Types {
    /// Whether this frame's type was asked for.
    fn keeps(&self, event: &ServerEvent) -> bool {
        self.0.is_empty() || self.0.contains(&RunEventType::of(event))
    }
}

/// The resolver state behind the `Subscription` type.
pub(crate) struct Subscription_;

/// The live side: what the fleet is doing as it does it, over a WebSocket at
/// `/ws/graphql`.
///
/// One long-lived stream in place of polling, narrowed server-side to the runs
/// and the frame types a client actually renders. Delivery is at-most-once, so
/// a stream is how a client stays current and not how it reconstructs the
/// past: for that, read the run.
#[Subscription(name = "Subscription")]
impl Subscription_ {
    /// The daemon broadcast, filtered server-side.
    ///
    /// Pass `types` to name only the frames you render. Pass `runId` or
    /// `runIds` to narrow to particular runs, and `includeDescendants` to have
    /// their sub-agents included as they spawn, which is what a fan-out needs:
    /// without it a client has to re-query the tree and re-subscribe as it
    /// grows. Pass no scope and it is every run, like `/ws`.
    ///
    /// Delivery is at-most-once. A dropped connection delivers nothing until
    /// the client reconnects, and a subscription that falls behind gets an
    /// `EventsDropped` frame saying how many it missed, which is the cue to
    /// re-read whatever it renders.
    async fn events(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Only these frame types; omitted or empty means all of them.")]
        types: Option<Vec<RunEventType>>,
        #[graphql(desc = "Narrow to one run.")] run_id: Option<String>,
        #[graphql(desc = "Narrow to a set of runs.")] run_ids: Option<Vec<String>>,
        #[graphql(
            desc = "Include the sub-agents of the runs in scope, as they spawn.",
            default = false
        )]
        include_descendants: bool,
    ) -> impl Stream<Item = RunEvent> + use<> {
        let state = ctx.data_unchecked::<AppState>();
        let mut scope = Scope {
            runs: run_id
                .into_iter()
                .chain(run_ids.into_iter().flatten())
                .collect(),
            include_descendants,
        };
        let wanted = Types(types.into_iter().flatten().collect());
        // Subscribed before anything is awaited, so a frame sent while the
        // stream is being set up is not missed.
        BroadcastStream::new(state.event_tx.subscribe()).filter_map(move |frame| {
            let kept = match frame {
                Ok(event) => (wanted.keeps(&event) && scope.keeps(&event)).then(|| event.into()),
                // The broadcast is bounded, and a receiver that cannot keep up
                // is skipped past rather than allowed to hold the fan-out.
                // Saying so is the difference between a quiet run and a missed
                // one.
                Err(BroadcastStreamRecvError::Lagged(missed)) => {
                    let missed = i64::try_from(missed).unwrap_or(i64::MAX);
                    Some(RunEvent::EventsDropped(EventsDropped {
                        count: BigInt(missed),
                    }))
                }
            };
            std::future::ready(kept)
        })
    }
}

#[cfg(test)]
#[path = "subscription_tests.rs"]
mod tests;
