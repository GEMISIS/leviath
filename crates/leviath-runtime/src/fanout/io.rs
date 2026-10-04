//! What a fan-out reads and copies, done off the tick.
//!
//! Two of a fan-out's steps touch the disk. Checking a split's items against
//! its worker's inputs reads the worker's blueprint (and, for a worker found
//! by query, walks the agents directory); handing a finished worker's files up
//! to its parent copies their bytes from the worker's store into the
//! parent's. The world decides that each happens and what its result means.
//! The I/O itself runs on the world's runtime, supervised, and comes back on
//! a channel a system drains: the items are checked when the inputs land,
//! and the parts join the parent's fan-out when the copy does.
//!
//! A world with no runtime ([`FanOutIo`] absent, a world assembled by hand)
//! does each in place.

use std::sync::Arc;

use bevy_ecs::prelude::*;
use leviath_core::mime::{BlobStore, InboundPart, MimeRegistry, Part};
use leviath_core::output::Artifact;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::*;

/// Where a fan-out's reads and copies run, and the channels they land on.
#[derive(Resource)]
pub(crate) struct FanOutIo {
    /// Where a finished copy is reported.
    hand_ups: UnboundedSender<HandedUp>,
    /// Finished copies, for the next pass of the collector.
    handed_up: UnboundedReceiver<HandedUp>,
    /// Where a finished read of a worker's inputs is reported.
    inputs: UnboundedSender<InputsRead>,
    /// Finished reads, for the next pass of the fan-out starter.
    inputs_read: UnboundedReceiver<InputsRead>,
    /// The runtime they run on.
    runtime: tokio::runtime::Handle,
    /// Wakes the tick loop when one lands.
    wake: Arc<Notify>,
}

impl FanOutIo {
    /// Reads and copies on `runtime`, waking the loop through `wake`.
    pub(crate) fn new(runtime: tokio::runtime::Handle, wake: Arc<Notify>) -> Self {
        let (hand_ups, handed_up) = tokio::sync::mpsc::unbounded_channel();
        let (inputs, inputs_read) = tokio::sync::mpsc::unbounded_channel();
        Self {
            hand_ups,
            handed_up,
            inputs,
            inputs_read,
            runtime,
            wake,
        }
    }
}

/// Copying one finished worker's files up to its parent: everything the copy
/// needs, read from the world when the worker is reaped.
pub(crate) struct HandUp {
    /// The parent the files go to.
    parent: Entity,
    /// The worker's work item, which names each part.
    item_id: String,
    /// The worker's run, whose store holds the bytes.
    worker_run: String,
    /// The parent's run, which stores them again.
    parent_run: String,
    /// The files the worker handed back with a hash to read them by.
    artifacts: Vec<Artifact>,
    /// The store both runs keep their bytes in.
    store: Arc<dyn BlobStore>,
    /// The parent's registry, which types and checks each part.
    registry: Arc<MimeRegistry>,
    /// The part ceilings.
    limits: MimeLimits,
}

/// The files a finished worker handed up, stored under its parent.
pub(crate) struct HandedUp {
    /// The parent they are for.
    pub(crate) parent: Entity,
    /// The parts, in the order the worker named them.
    pub(crate) parts: Vec<Part>,
}

impl HandUp {
    /// What handing `worker`'s files up to `parent` takes, or `None` when it
    /// has nothing to hand up (no output, no stored file) or the world keeps
    /// no store or registry to do it with.
    pub(crate) fn of(world: &World, parent: Entity, worker: &ActiveWorker) -> Option<Self> {
        let output = world.get::<crate::persistence::FinalOutput>(worker.entity)?;
        let artifacts: Vec<Artifact> = output
            .0
            .artifacts
            .iter()
            .filter(|a| !a.sha256.is_empty())
            .cloned()
            .collect();
        if artifacts.is_empty() {
            return None;
        }
        let store = world.get_resource::<BlobStoreHandle>()?.0.clone();
        let registry = world
            .get::<RunMimeRegistry>(parent)
            .map(RunMimeRegistry::registry)
            .or_else(|| {
                world
                    .get_resource::<MimeRegistryHandle>()
                    .map(|r| r.0.clone())
            })?;
        Some(Self {
            parent,
            item_id: worker.item_id.clone(),
            worker_run: worker.run_id.clone(),
            parent_run: world
                .get::<crate::persistence::RunMetadata>(parent)
                .map(|m| m.run_id.clone())
                .unwrap_or_default(),
            artifacts,
            store,
            registry,
            limits: world
                .get_resource::<MimeLimits>()
                .copied()
                .unwrap_or_default(),
        })
    }

    /// Copy the files: read each from the worker's store and store it again
    /// under the parent as a part named `<item>/<artifact>`. The store is
    /// content-addressed, so a file two workers both produced is one file on
    /// disk. A file the store no longer holds, or one over the part ceiling,
    /// is left out with a warning rather than failing the merge.
    pub(crate) fn run(self) -> HandedUp {
        let sink = PartSink {
            store: self.store.as_ref(),
            registry: &self.registry,
            run_id: &self.parent_run,
            max_part_bytes: self.limits.max_part_bytes,
            inline_text_bytes: self.limits.inline_text_bytes,
        };
        let parts = self
            .artifacts
            .iter()
            .filter_map(|artifact| {
                let name = format!("{}/{}", self.item_id, artifact.name);
                let stored = self
                    .store
                    .read(&self.worker_run, &artifact.sha256)
                    .map_err(|e| e.to_string())
                    .and_then(|bytes| {
                        sink.store_part(
                            &InboundPart::from_bytes(name.clone(), bytes.to_vec())
                                .typed(artifact.mime_type.clone()),
                        )
                    });
                match stored {
                    Ok(part) => Some(part),
                    Err(why) => {
                        tracing::warn!(item = %self.item_id, part = %name, "[mime] worker artifact not handed up: {why}");
                        None
                    }
                }
            })
            .collect();
        HandedUp {
            parent: self.parent,
            parts,
        }
    }
}

/// Hand `job`'s files up. On the world's runtime when it has one, landing on
/// a later pass (`None` now); otherwise in place, returned to land now.
pub(crate) fn hand_up(world: &World, job: HandUp) -> Option<HandedUp> {
    let Some(io) = world.get_resource::<FanOutIo>() else {
        return Some(job.run());
    };
    let (tx, wake) = (io.hand_ups.clone(), io.wake.clone());
    let (lost_tx, lost_wake, parent) = (tx.clone(), wake.clone(), job.parent);
    // Supervised: the parent's fan-out waits for this copy, and one that died
    // without reporting would hold it open for good. A lost copy hands up
    // nothing, as a file the store no longer holds does.
    crate::lane_supervisor::spawn_supervised(
        &io.runtime,
        "fan-out hand-up",
        async move {
            let _ = tx.send(job.run());
            wake.notify_one();
        },
        move |_| {
            let _ = lost_tx.send(HandedUp {
                parent,
                parts: Vec::new(),
            });
            lost_wake.notify_one();
        },
    );
    None
}

/// Every copy that has landed since the last pass.
pub(crate) fn drain_hand_ups(world: &mut World) -> Vec<HandedUp> {
    let mut landed = Vec::new();
    if let Some(mut io) = world.get_resource_mut::<FanOutIo>() {
        while let Ok(one) = io.handed_up.try_recv() {
            landed.push(one);
        }
    }
    landed
}

/// A `fan_out` call whose worker's inputs are being read off the tick. The
/// call waits, as [`PendingFanOut`], until they land.
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct ReadingWorkerInputs;

/// The inputs a waiting `fan_out` call's worker declares, read off the tick:
/// `None` when they could not be read, and each worker's own start checks
/// them then.
#[derive(Component, Debug, Clone)]
pub(crate) struct WorkerInputs(pub(crate) Option<Vec<InputDecl>>);

/// A read of a worker's inputs that has finished.
pub(crate) struct InputsRead {
    /// The agent whose `fan_out` call it is for.
    entity: Entity,
    /// What was read.
    decls: Option<Vec<InputDecl>>,
}

/// The blueprint whose inputs a `fan_out` call's worker takes: one it names,
/// or one an installed blueprint's query finds.
pub(crate) enum WorkerBlueprint {
    /// Named outright.
    Named(SpawnSource),
    /// Found by a query, through the installed spawner.
    Query(String),
}

impl WorkerBlueprint {
    /// The blueprint `worker` names, or `None` for a worker stage, which
    /// runs its own run's graph.
    pub(crate) fn of(worker: &WorkerSource) -> Option<Self> {
        match worker {
            WorkerSource::Stage(_) => None,
            WorkerSource::Query(query) => Some(Self::Query(query.clone())),
            WorkerSource::Blueprint(b) => Some(Self::Named(SpawnSource::Blueprint(b.clone()))),
            WorkerSource::BlueprintFile(path) => {
                Some(Self::Named(SpawnSource::BlueprintFile(path.clone())))
            }
        }
    }
}

/// Read the inputs of `worker` for `entity`'s `fan_out` call, through the
/// installed spawner. On the world's runtime when it has one (`None` now; the
/// read lands as [`WorkerInputs`] on a later pass); otherwise in place,
/// returned now. A world with no spawner reads nothing.
pub(crate) fn read_inputs(
    world: &World,
    entity: Entity,
    worker: WorkerBlueprint,
) -> Option<Option<Vec<InputDecl>>> {
    let Some(spawner) = world
        .get_resource::<FanOutSpawnerRes>()
        .map(|r| r.0.clone())
    else {
        return Some(None);
    };
    let read = move || {
        let source = match worker {
            WorkerBlueprint::Named(source) => source,
            WorkerBlueprint::Query(query) => {
                SpawnSource::Blueprint(spawner.find_worker(&query).ok()?)
            }
        };
        spawner.worker_inputs(&source)
    };
    let Some(io) = world.get_resource::<FanOutIo>() else {
        return Some(read());
    };
    let (tx, wake) = (io.inputs.clone(), io.wake.clone());
    let (lost_tx, lost_wake) = (tx.clone(), wake.clone());
    // Supervised: the call waits for this read. One that died reads as
    // inputs that could not be read, which each worker's start checks.
    crate::lane_supervisor::spawn_supervised(
        &io.runtime,
        "fan-out inputs",
        async move {
            let _ = tx.send(InputsRead {
                entity,
                decls: read(),
            });
            wake.notify_one();
        },
        move |_| {
            let _ = lost_tx.send(InputsRead {
                entity,
                decls: None,
            });
            lost_wake.notify_one();
        },
    );
    None
}

/// Place every read of a worker's inputs that has landed on the agent it is
/// for, as [`WorkerInputs`], so its waiting `fan_out` call goes on.
pub(crate) fn land_inputs(world: &mut World) {
    let mut landed = Vec::new();
    if let Some(mut io) = world.get_resource_mut::<FanOutIo>() {
        while let Ok(one) = io.inputs_read.try_recv() {
            landed.push(one);
        }
    }
    // An agent that went meanwhile has no call left to check.
    for InputsRead { entity, decls } in landed {
        let _ = world.get_entity_mut(entity).map(|mut agent| {
            agent
                .remove::<ReadingWorkerInputs>()
                .insert(WorkerInputs(decls));
        });
    }
}
