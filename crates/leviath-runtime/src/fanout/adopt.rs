//! A fan-out worker its parent never recorded, found after a restart.
//!
//! Starting a worker records its run file first and links it to its parent
//! when it is placed; the parent's own file records the link when the parent
//! next takes a step. A daemon that stops in between leaves a worker whose
//! file says which parent and which work item it is for, and a parent whose
//! file still has that item queued. Brought back as they are, the parent
//! would start the item again and the worker would run it too.
//!
//! So when a restart finds such a worker, the parent's fan-out decides what
//! it is: the worker for an item it still has queued is adopted, as the
//! item's running worker; one for an item it is not waiting on (already
//! running under another worker, finished, or a fan-out that ended) is
//! cancelled. Either way the item runs once.

use super::*;

/// What became of a worker its parent never recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnrecordedWorker {
    /// It is the running worker for the item its parent still had queued.
    Adopted,
    /// Its parent is not waiting on its item, so it was cancelled.
    Cancelled,
}

/// Settle `worker`, a run whose spec names `parent` as the run that started
/// it but which `parent` never recorded among its children. `None` when it
/// is not a fan-out worker (it names no work item), which is left as it is.
pub fn settle_unrecorded_worker(
    world: &mut World,
    parent: Entity,
    worker: Entity,
) -> Option<UnrecordedWorker> {
    let spec = world.get::<RunSpecC>(worker)?.0.clone();
    let item_id = spec.delivery.metadata.get(WORK_ITEM_LABEL)?.clone();
    let queued = world.get_mut::<FanOutWaiting>(parent).and_then(|mut w| {
        let at = w.pending.iter().position(|item| item.id == item_id)?;
        w.pending.remove(at);
        w.active.push(ActiveWorker {
            item_id: item_id.clone(),
            entity: worker,
            run_id: spec.run_id.to_string(),
        });
        Some(())
    });
    let worker_id = spec.run_id.to_string();
    let Some(()) = queued else {
        tracing::warn!(
            worker = %worker_id,
            item = %item_id,
            "a fan-out worker its parent never recorded is for an item the parent is not waiting on; cancelling it"
        );
        set_status(world, worker, AgentStatus::Cancelled);
        return Some(UnrecordedWorker::Cancelled);
    };
    tracing::info!(
        worker = %worker_id,
        item = %item_id,
        "adopting a fan-out worker its parent never recorded"
    );
    let max_child_depth = world
        .get::<RunSpecC>(parent)
        .and_then(|p| p.0.graph.max_child_depth.map(usize::from))
        .unwrap_or(DEFAULT_FANOUT_DEPTH);
    let parent_agent_id = world
        .get::<AgentState>(parent)
        .map(|s| s.agent_id.clone())
        .unwrap_or_default();
    world.entity_mut(worker).insert(ParentRef {
        parent_entity: parent,
        parent_agent_id,
        depth: usize::from(spec.placement.depth),
    });
    match world.get_mut::<SubAgentChildren>(parent) {
        Some(mut kids) => kids.children.push(worker),
        None => {
            world.entity_mut(parent).insert(SubAgentChildren {
                children: vec![worker],
                max_child_depth,
            });
        }
    }
    world
        .get_mut::<AgentState>(parent)
        .expect("a fan-out parent always has AgentState")
        .spawned_children_ids
        .push(worker_id);
    Some(UnrecordedWorker::Adopted)
}
