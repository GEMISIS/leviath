//! Starting a fan-out worker without stopping the world to do it.
//!
//! Starting a worker means resolving its blueprint, recording its run file and
//! binding it to this machine: reads, writes and, for a worker found by query,
//! a walk of the agents directory. None of it belongs on the tick. So
//! [`fan_out_collect`](super::fan_out_collect) only decides that an item
//! starts (the depth and budget checks read the world), and hands the rest to
//! the spawner as a [`WorkerPrep`], which runs on the world's runtime. What it
//! resolves to comes back as a [`WorkerLanded`] and is placed by a later pass:
//! an insert, which cannot fail, and the links to its parent.
//!
//! Until it lands, the item is *starting*: it counts against the concurrency
//! cap and the run's agent budget like a running worker, and a parent that is
//! saved meanwhile saves it as still to start.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bevy_ecs::prelude::*;
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::*;

/// Puts a prepared worker in the world and returns its entity. Cannot fail:
/// everything that could was done preparing it.
pub type PlaceWorker = Box<dyn FnOnce(&mut World) -> Entity + Send>;

/// A worker being prepared off the tick: resolves to how to place it, or why
/// it could not start (recorded as that item's failure).
pub type WorkerPrep = Pin<Box<dyn Future<Output = Result<PlaceWorker, String>> + Send>>;

/// A worker start that has finished off the tick.
pub(crate) struct WorkerLanded {
    /// The parent that started it.
    parent: Entity,
    /// The work item it is for.
    item_id: String,
    /// Where it sits in the tree, as worked out when it began.
    depths: Depths,
    /// How to place it, or why it did not start.
    result: Result<PlaceWorker, String>,
}

/// The depth a worker starts at, and the deepest a child of its tree may be.
#[derive(Debug, Clone, Copy)]
pub(super) struct Depths {
    pub(super) child: usize,
    max: usize,
}

/// A worker start that may go ahead: what preparing it is, and where it will
/// sit.
pub(super) struct Start {
    prep: WorkerPrep,
    depths: Depths,
}

/// Where worker starts run and report: the world's runtime, and the channel
/// [`fan_out_collect`](super::fan_out_collect) drains. Absent in a world
/// assembled by hand, which runs a start in place when it needs no waiting.
#[derive(Resource)]
pub(crate) struct WorkerStarts {
    /// Where a finished start is reported.
    tx: UnboundedSender<WorkerLanded>,
    /// Finished starts, for the next pass.
    rx: UnboundedReceiver<WorkerLanded>,
    /// The runtime starts run on.
    runtime: tokio::runtime::Handle,
    /// Wakes the tick loop when a start lands.
    wake: Arc<Notify>,
}

impl WorkerStarts {
    /// Starts on `runtime`, waking the loop through `wake`.
    pub(crate) fn new(runtime: tokio::runtime::Handle, wake: Arc<Notify>) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            tx,
            rx,
            runtime,
            wake,
        }
    }
}

/// Decide whether `item` may start under `parent`, and if so, what preparing
/// it is. `starting` is how many of the parent's workers are still being
/// prepared, which count against the run's agent budget as if they were
/// already running.
pub(super) fn begin(
    world: &World,
    parent: Entity,
    config: &FanOutDef,
    item: &WorkItem,
    starting: usize,
) -> Result<Start, String> {
    let spec = world
        .get::<RunSpecC>(parent)
        .map(|s| s.0.clone())
        .ok_or_else(|| "fan-out parent has no run spec".to_string())?;
    let depths = depths(world, parent, &spec, starting)?;
    let parent_depth = depths.child - 1;
    let spawner = world
        .get_resource::<FanOutSpawnerRes>()
        .map(|r| r.0.clone())
        .ok_or_else(|| "no fan-out spawner installed".to_string())?;
    let (config, item) = (config.clone(), item.clone());
    let prep: WorkerPrep = Box::pin(async move {
        // Off the tick from here: a query is a walk of the agents directory.
        let source = match &config.worker {
            WorkerSource::Blueprint(blueprint) => SpawnSource::Blueprint(blueprint.clone()),
            WorkerSource::BlueprintFile(path) => SpawnSource::BlueprintFile(path.clone()),
            WorkerSource::Stage(_) => spec.same_graph_source(),
            WorkerSource::Query(query) => SpawnSource::Blueprint(spawner.find_worker(query)?),
        };
        let (request, caller) = items::worker_request(&spec, &config, &item, source, parent_depth);
        spawner.prepare_worker(request, caller).await
    });
    Ok(Start { prep, depths })
}

/// Where a worker of `parent` (whose spec is `spec`) sits in the tree, or
/// why no worker of it may start now: the tree is as deep as it may go, or
/// the run has as many agents as its ceiling allows. `starting` is how many
/// of the parent's workers are still being prepared, which count against
/// the run's agent budget as if they were already running.
pub(super) fn depths(
    world: &World,
    parent: Entity,
    spec: &crate::spec::run_spec::RunSpec,
    starting: usize,
) -> Result<Depths, String> {
    let max_depth = world
        .get::<SubAgentChildren>(parent)
        .map(|k| k.max_child_depth)
        .or_else(|| spec.graph.max_child_depth.map(usize::from))
        .unwrap_or(DEFAULT_FANOUT_DEPTH);
    let parent_depth = world.get::<ParentRef>(parent).map_or(0, |p| p.depth);
    let child_depth = parent_depth + 1;
    if child_depth > max_depth {
        return Err(format!(
            "fan-out worker depth limit ({max_depth}) reached; not spawning"
        ));
    }
    // The run's own ceiling, beside the depth one. Refusing here rather than at
    // the split means the items already started keep running and the merge still
    // happens on what came back: a run that stopped widening is a cheaper answer,
    // not a failure.
    let budget = world.get_resource::<FanOutBudget>().map_or(0, |b| b.0);
    let live = run_tree_size(world, parent) + starting;
    if budget > 0 && live >= budget {
        return Err(format!(
            "this run already has {live} agents and its ceiling is {budget} \
             ([limits] max_agents_per_run); not spawning another"
        ));
    }
    Ok(Depths {
        child: child_depth,
        max: max_depth,
    })
}

/// Run `prep` for `parent`'s item `item_id`. On the world's runtime when it
/// has one, reporting when it lands; otherwise in place, which only a start
/// that needs no waiting can be, and the result is returned to land now.
pub(super) fn launch(
    world: &World,
    parent: Entity,
    item_id: String,
    start: Start,
) -> Option<WorkerLanded> {
    let Start { mut prep, depths } = start;
    let Some(starts) = world.get_resource::<WorkerStarts>() else {
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let result = match prep.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(result) => result,
            std::task::Poll::Pending => {
                Err("a worker start that has to wait needs the world's runtime".to_string())
            }
        };
        return Some(WorkerLanded {
            parent,
            item_id,
            depths,
            result,
        });
    };
    let (tx, wake) = (starts.tx.clone(), starts.wake.clone());
    let (lost_tx, lost_wake, lost_item) = (tx.clone(), wake.clone(), item_id.clone());
    // Supervised: the parent waits on this item as starting, and a start that
    // died without reporting would hold its fan-out open for good.
    crate::lane_supervisor::spawn_supervised(
        &starts.runtime,
        "fan-out start",
        async move {
            let result = prep.await;
            let _ = tx.send(WorkerLanded {
                parent,
                item_id,
                depths,
                result,
            });
            wake.notify_one();
        },
        move |message| {
            let _ = lost_tx.send(WorkerLanded {
                parent,
                item_id: lost_item,
                depths,
                result: Err(message),
            });
            lost_wake.notify_one();
        },
    );
    None
}

/// Every start that has landed since the last pass.
pub(super) fn drain(world: &mut World) -> Vec<WorkerLanded> {
    let mut landed = Vec::new();
    if let Some(mut starts) = world.get_resource_mut::<WorkerStarts>() {
        while let Ok(one) = starts.rx.try_recv() {
            landed.push(one);
        }
    }
    landed
}

impl WorkerLanded {
    /// The parent the start was for.
    pub(super) fn parent(&self) -> Entity {
        self.parent
    }
}

/// Place a landed start under `parent`, whose fan-out is `w`: the worker joins
/// the running ones, linked to its parent, or the item is recorded as failed.
pub(super) fn land(world: &mut World, parent: Entity, w: &mut FanOutWaiting, landed: WorkerLanded) {
    let (item_id, placed) = place(world, parent, landed);
    w.starting.retain(|item| item.id != item_id);
    match placed {
        Ok(child) => {
            // Capture the worker's run-id so the waiting state persists.
            let run_id = world
                .get::<crate::persistence::RunMetadata>(child)
                .map(|m| m.run_id.clone())
                .unwrap_or_default();
            w.active.push(ActiveWorker {
                item_id,
                entity: child,
                run_id,
            });
        }
        Err(message) => w.failures.push((item_id, message)),
    }
}

/// Place a landed start under `parent`: its item, with the worker linked to
/// its parent, or why it did not start.
pub(super) fn place(
    world: &mut World,
    parent: Entity,
    landed: WorkerLanded,
) -> (String, Result<Entity, String>) {
    let WorkerLanded {
        item_id,
        result,
        depths,
        ..
    } = landed;
    let placed = result.map(|place| {
        let child = place(world);
        link(world, parent, child, depths, true);
        tracing::info!(item = %item_id, "fan-out worker started");
        child
    });
    (item_id, placed)
}

/// A start that landed after its parent stopped waiting for it: the worker
/// is placed, so its run file says how it ended, and cancelled at once.
pub(super) fn abandon(world: &mut World, landed: WorkerLanded) {
    if let Ok(place) = landed.result {
        let child = place(world);
        set_status(world, child, AgentStatus::Cancelled);
    }
}

/// Link a placed worker to `parent` (`ParentRef` + `SubAgentChildren`), record
/// it on the parent's state so the tree survives a restart, and, with
/// `seed`, seed its context from the parent per any declared transform.
pub(super) fn link(world: &mut World, parent: Entity, child: Entity, depths: Depths, seed: bool) {
    let parent_agent_id = world
        .get::<AgentState>(parent)
        .map(|s| s.agent_id.clone())
        .unwrap_or_default();
    world.entity_mut(child).insert(ParentRef {
        parent_entity: parent,
        parent_agent_id,
        depth: depths.child,
    });
    match world.get_mut::<SubAgentChildren>(parent) {
        Some(mut kids) => kids.children.push(child),
        None => {
            world.entity_mut(parent).insert(SubAgentChildren {
                children: vec![child],
                max_child_depth: depths.max,
            });
        }
    }
    // A freshly placed worker always has run metadata; its parent always has
    // state.
    let worker_id = world
        .get::<crate::persistence::RunMetadata>(child)
        .expect("a fan-out worker always has run metadata")
        .run_id
        .clone();
    world
        .get_mut::<AgentState>(parent)
        .expect("a fan-out parent always has AgentState")
        .spawned_children_ids
        .push(worker_id);
    if seed {
        crate::context_transform::apply_context_transforms(
            world,
            crate::world::AgentId::in_world(world, parent),
            crate::world::AgentId::in_world(world, child),
        );
    }
}
