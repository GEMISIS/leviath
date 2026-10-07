//! What happens to a run between its snapshots, gathered in the world.
//!
//! A model call, a tool batch and each call's result, an answer, a move, a
//! context change: whatever sees one sends a [`RunRecord`] through the
//! [`JournalSender`] to the world's [`JournalInbox`], from a system or from a
//! task off the tick. The persist system drains the inbox every tick and folds
//! each record into the events of its run's next step, pairing a model call's
//! attempt with its bill through [`RunJournals`]. What it hands the lane is a
//! whole step, so the lane only writes.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use bevy_ecs::prelude::{Component, Entity, Resource, World};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, WeakUnboundedSender};
use tokio::sync::{Notify, oneshot};

use crate::persistence_bridge::{Appended, PersistMsg};
use crate::runfile::events::{Answered, push_events};
use crate::runfile::lane::{RunFileStep, RunNow};
use crate::runfile::record::RunRecord;
use crate::state::RunEvent;
use crate::state::files::BlobFile;

/// One record on its way to the world, for the run's next step.
#[derive(Debug)]
pub(crate) struct Journaled {
    /// The run it belongs to.
    pub run_id: String,
    /// What happened. Boxed: the channel moves these by value, and a record
    /// can carry a whole request.
    pub record: Box<RunRecord>,
    /// Who waits to hear where the step holding it landed: the dispatch-side
    /// barrier that keeps a batch's record ahead of the batch's side effects.
    pub ack: Option<oneshot::Sender<Appended>>,
}

/// Where anything that sees a run do something sends the record of it.
#[derive(Resource, Clone)]
pub(crate) struct JournalSender {
    tx: UnboundedSender<Journaled>,
    /// The world's wake, for a handle given to a task off the tick.
    world: Option<Arc<Notify>>,
    /// What this handle wakes when it sends: nothing for a system, which runs
    /// on a tick that drains the inbox, and the world for a task.
    wake: Option<Arc<Notify>>,
}

impl JournalSender {
    /// A sender into `tx`, whose [`waking`](Self::waking) handles wake
    /// `world`.
    pub(crate) fn new(tx: UnboundedSender<Journaled>, world: Option<Arc<Notify>>) -> Self {
        Self {
            tx,
            world,
            wake: None,
        }
    }

    /// A handle for a task off the tick, which wakes the world with each
    /// record so it reaches the run's file without waiting for something else
    /// to tick the world: a call that finishes while the rest of its batch
    /// runs is on disk before the batch ends.
    pub(crate) fn waking(&self) -> Self {
        Self {
            wake: self.world.clone(),
            ..self.clone()
        }
    }

    /// Send `record` for `run_id`'s next step. A world that has stopped takes
    /// nothing.
    pub(crate) fn record(&self, run_id: &str, record: RunRecord) {
        self.send(run_id, record, None);
    }

    /// Send `record` for `run_id`'s next step, and hear where that step
    /// landed.
    pub(crate) fn record_acked(
        &self,
        run_id: &str,
        record: RunRecord,
    ) -> oneshot::Receiver<Appended> {
        let (ack, landed) = oneshot::channel();
        self.send(run_id, record, Some(ack));
        landed
    }

    fn send(&self, run_id: &str, record: RunRecord, ack: Option<oneshot::Sender<Appended>>) {
        let _ = self.tx.send(Journaled {
            run_id: run_id.to_string(),
            record: Box::new(record),
            ack,
        });
        if let Some(wake) = &self.wake {
            wake.notify_one();
        }
    }

    /// A handle that does not keep the inbox open.
    pub(crate) fn downgrade(&self) -> WeakUnboundedSender<Journaled> {
        self.tx.downgrade()
    }
}

/// The world's end of the [`JournalSender`].
#[derive(Resource)]
pub(crate) struct JournalInbox(pub UnboundedReceiver<Journaled>);

/// Per run, the model calls that answered and wait for the usage record that
/// bills them: the one thing about a run's records the world carries from one
/// tick to the next.
#[derive(Resource, Default)]
pub(crate) struct RunJournals(HashMap<String, Answered>);

/// The stored parts a run has named in its file, in the order they first
/// appeared. Placed from the run's state when it is inserted, and added to
/// from its context each time its state is recorded, so a part that has left
/// the context stays named.
#[derive(Component, Debug, Clone, Default)]
pub(crate) struct RunBlobs(pub Vec<BlobFile>);

/// What happened to one run since its last step, folded from its records.
#[derive(Default)]
pub(crate) struct Happened {
    /// The events of its next step.
    pub events: Vec<RunEvent>,
    /// Who waits to hear where that step landed.
    pub acks: Vec<oneshot::Sender<Appended>>,
    /// Whether a call of a batch still running finished: its state then has
    /// to be recorded with the step, so the file holds the call as done.
    pub landed: bool,
}

/// Take every record the inbox holds and fold each into its run's events.
pub(crate) fn drain(world: &mut World) -> BTreeMap<String, Happened> {
    let mut records = Vec::new();
    if let Some(mut inbox) = world.get_resource_mut::<JournalInbox>() {
        while let Ok(record) = inbox.0.try_recv() {
            records.push(record);
        }
    }
    let mut happened: BTreeMap<String, Happened> = BTreeMap::new();
    if records.is_empty() {
        return happened;
    }
    let mut journals = world.get_resource_or_insert_with(RunJournals::default);
    for Journaled {
        run_id,
        record,
        ack,
    } in records
    {
        let answered = journals.0.entry(run_id.clone()).or_default();
        let run = happened.entry(run_id).or_default();
        run.landed |= matches!(*record, RunRecord::ToolCallDone { .. });
        push_events(&mut run.events, answered, &record);
        run.acks.extend(ack);
    }
    journals.0.retain(|_, waiting| !waiting.is_empty());
    happened
}

/// The run on `entity` as its file records it now: its state, with every
/// stored part its context holds named, and the spec it was placed from.
/// `None` for a run not placed from a spec, or one whose state is not in
/// memory.
pub(crate) fn run_now(world: &mut World, entity: Entity) -> Option<RunNow> {
    let spec = world.get::<crate::insert::RunSpecC>(entity)?.0.clone();
    let mut state = crate::state::inspect::inspect(world, entity)?;
    let mut run = world.entity_mut(entity);
    let mut blobs = run.entry::<RunBlobs>().or_default().into_mut();
    crate::state::files::note_blobs(&mut blobs.0, &state.context);
    state.blobs = blobs.0.clone();
    Some(RunNow { spec, state })
}

/// Hand the lane a step for each run in `happened`: its events, with its
/// state now when a call of its batch finished, and the state its file last
/// recorded otherwise.
pub(crate) fn send(world: &mut World, happened: BTreeMap<String, Happened>) {
    let at = chrono::Utc::now().timestamp();
    for (run_id, run) in happened {
        let now = match run.landed {
            true => live(world, &run_id).and_then(|entity| run_now(world, entity)),
            false => None,
        };
        let step = RunFileStep {
            run_id,
            now,
            at,
            events: run.events,
            acks: run.acks,
        };
        match world.get_resource::<super::PersistenceStage>() {
            Some(lane) => {
                let _ = lane.0.send(PersistMsg::Step(Box::new(step)));
            }
            None => {
                for ack in step.acks {
                    let _ = ack.send(Appended::NoJournal);
                }
            }
        }
    }
}

/// Fold what the inbox holds and hand it to the lane: how what happened
/// after a world's last tick reaches the files as it stops. The runs in
/// `settled` had a batch's calls settled as it stopped, so their state goes
/// too.
pub(crate) fn flush(world: &mut World, settled: &[String]) {
    let mut happened = drain(world);
    for run_id in settled {
        happened.entry(run_id.clone()).or_default().landed = true;
    }
    send(world, happened);
}

/// The entity of the run `run_id`, when the world holds it.
fn live(world: &mut World, run_id: &str) -> Option<Entity> {
    world
        .query::<(Entity, &crate::persistence::RunMetadata)>()
        .iter(world)
        .find(|(_, md)| md.run_id == run_id)
        .map(|(entity, _)| entity)
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
