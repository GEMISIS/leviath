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
//! it is. A worker is adopted only when it is provably the worker the parent
//! would start for an item it still has queued: started under this parent,
//! for that item, from the blueprint or stage the fan-out runs, at the depth
//! a start would place it, and with the inputs, model, output shape and
//! workdir the parent would ask for. Adopting it does what a start does: the
//! item's running worker, linked to its parent and counted in its tree, its
//! context seeded from the parent's when its file holds only its first
//! record. Any other worker (for an item the parent is not waiting on, or
//! one that does not match its item) is cancelled, and the item runs fresh.
//! Either way the item runs once.

use super::*;
use crate::spec::inputs::{CheckCtx, check_inputs};
use crate::spec::run_spec::{RunSpec, SpecOrigin};

/// What became of a worker its parent never recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnrecordedWorker {
    /// It is the running worker for the item its parent still had queued.
    Adopted,
    /// It is not the worker its parent would start for its item, so it was
    /// cancelled.
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
    let worker_id = spec.run_id.to_string();
    let depths = match proven_worker(world, parent, &spec, &item_id) {
        Ok(depths) => depths,
        Err(why) => {
            tracing::warn!(
                worker = %worker_id,
                item = %item_id,
                why = %why,
                "a fan-out worker its parent never recorded is not the worker for its item; cancelling it"
            );
            set_status(world, worker, AgentStatus::Cancelled);
            return Some(UnrecordedWorker::Cancelled);
        }
    };
    tracing::info!(
        worker = %worker_id,
        item = %item_id,
        "adopting a fan-out worker its parent never recorded"
    );
    let mut w = world
        .get_mut::<FanOutWaiting>(parent)
        .expect("a proven worker's parent is fanning out");
    w.pending.retain(|item| item.id != item_id);
    w.active.push(ActiveWorker {
        item_id,
        entity: worker,
        run_id: worker_id.clone(),
    });
    // A worker whose file holds only the record its start wrote was never
    // seeded from its parent's context: that happens as it is placed, and
    // its next record would have held it.
    let first_record = world
        .get_resource::<crate::pipeline::PersistLaneHealth>()
        .map_or(0, |lane| lane.0.step_of(&worker_id))
        == 0;
    starts::link(world, parent, worker, depths, first_record);
    Some(UnrecordedWorker::Adopted)
}

/// Where `worker` (whose spec is `spec`) sits under `parent` when it is the
/// worker `parent` would start now for its queued item `item_id`, or why it
/// is not.
fn proven_worker(
    world: &World,
    parent: Entity,
    spec: &RunSpec,
    item_id: &str,
) -> Result<starts::Depths, String> {
    let w = world
        .get::<FanOutWaiting>(parent)
        .ok_or("its parent is not fanning out")?;
    let item = w
        .pending
        .iter()
        .find(|item| item.id == item_id)
        .ok_or("its parent is not waiting on its item")?;
    let parent_spec = &world
        .get::<RunSpecC>(parent)
        .expect("a run fanning out was placed from its spec")
        .0;
    let depths = starts::depths(world, parent, parent_spec, w.starting.len())?;
    if spec.placement.parent.as_ref() != Some(&parent_spec.run_id)
        || usize::from(spec.placement.depth) != depths.child
    {
        return Err("it is not placed where a worker of its parent would be".to_string());
    }
    let source = expected_source(world, &w.config, parent_spec)?;
    let stage = match &w.config.worker {
        WorkerSource::Stage(stage) => Some(stage),
        WorkerSource::Blueprint(_) | WorkerSource::BlueprintFile(_) | WorkerSource::Query(_) => {
            None
        }
    };
    if !runs_from(&source, &spec.origin) || spec.placement.worker_stage.as_ref() != stage {
        return Err("it does not run what the fan-out starts".to_string());
    }
    let (request, _) = items::worker_request(parent_spec, &w.config, item, source, 0);
    let decls: Vec<InputDecl> = spec
        .graph
        .inputs
        .iter()
        .cloned()
        .map(|decl| InputDecl {
            required: false,
            ..decl
        })
        .collect();
    let inputs = check_inputs(&decls, &request.inputs, &CheckCtx { attachments: &[] }).ok();
    let asked = (
        inputs.as_ref(),
        request.model.as_ref(),
        request.output.as_ref(),
        request.workdir.as_ref(),
    );
    let has = (
        Some(&spec.inputs),
        spec.requested_model.as_ref(),
        spec.requested_output.as_ref(),
        Some(&spec.placement.workdir),
    );
    match asked == has {
        true => Ok(depths),
        false => Err("it was not started with what its item asks for".to_string()),
    }
}

/// What the fan-out `config` of a run whose spec is `parent` starts its
/// workers from, as a start would work it out.
fn expected_source(
    world: &World,
    config: &FanOutDef,
    parent: &RunSpec,
) -> Result<SpawnSource, String> {
    Ok(match &config.worker {
        WorkerSource::Stage(_) => parent.same_graph_source(),
        WorkerSource::Blueprint(blueprint) => SpawnSource::Blueprint(blueprint.clone()),
        WorkerSource::BlueprintFile(path) => SpawnSource::BlueprintFile(path.clone()),
        WorkerSource::Query(query) => SpawnSource::Blueprint(
            world
                .get_resource::<FanOutSpawnerRes>()
                .ok_or("no fan-out spawner installed to answer its query")?
                .0
                .find_worker(query)?,
        ),
    })
}

/// Whether a run that came from `origin` is one `source` starts. A
/// blueprint asked for without a revision is any revision of it.
fn runs_from(source: &SpawnSource, origin: &SpecOrigin) -> bool {
    match (source, origin) {
        (SpawnSource::Blueprint(want), SpecOrigin::Blueprint { blueprint, .. }) => {
            want.name == blueprint.name
                && want
                    .digest
                    .as_ref()
                    .is_none_or(|digest| Some(digest) == blueprint.digest.as_ref())
        }
        (SpawnSource::BlueprintFile(want), SpecOrigin::BlueprintFile { path, .. }) => want == path,
        (SpawnSource::Raw(_), SpecOrigin::Raw) => true,
        _ => false,
    }
}
