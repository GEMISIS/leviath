//! Restart recovery: the runs the daemon was running come back from their run
//! files when it starts, and a run it unloaded comes back when something asks
//! for it.
//!
//! A run's file holds everything about it: the spec it was resolved to, its
//! code and files, and the state it was last in. Bringing it back reads that,
//! binds the spec against this machine, and places it with `insert`. Nothing
//! is resolved again, so the run carries on with the models, tools, launch
//! policy and inputs it started with. A binding that fails (a provider that is
//! gone, an MCP server whose tools changed) is a refusal naming what changed,
//! recorded on the run's own file so it ends there instead of sitting as a run
//! that never moves. A model list a gateway has not answered yet does not
//! hold a run back: the run chose its models when it started.
//!
//! What the run was doing comes back with its state: a model call that was
//! out is made again, a tool batch in flight is dispatched again with the
//! results that came back carried over (a call that finished never runs
//! twice), a question put to a person or a stage checkpoint is asked again, a
//! choice of edge is asked again, and a fan-out picks its workers back up. A
//! run directory in the older many-file layout is converted to a run file
//! first, when this build carries the converter.

use std::path::Path;

use bevy_ecs::entity::Entity;
use leviath_runtime::restore::Resumable;
use leviath_runtime::spec::issues::SpawnIssues;
use leviath_runtime::state::RunStatus;
use leviath_runtime::world::{AgentId, PipelineWorld};

use crate::daemon::starter::DaemonStarter;

/// What a restart brought back: the `(run_id, entity)` pairs for the host to
/// map.
#[derive(Default)]
pub(crate) struct Recovered {
    pub reloaded: Vec<(String, AgentId)>,
}

/// Convert the old run in `dir` (every one under it, with `all`), looking its
/// stages up the way a new run of its graph would be resolved here.
fn convert_old(starter: &DaemonStarter, dir: &Path, all: bool) {
    let envs = |graph: &leviath_runtime::spec::graph::RunGraph| {
        starter.env_for_graph(graph, starter.config.current())
    };
    let agents = starter.agents_dir.as_deref();
    match all {
        true => crate::daemon::convert_old::convert_all(dir, agents, Some(&envs)),
        false => crate::daemon::convert_old::convert_one(dir, agents, Some(&envs)),
    }
}

/// Read the run in `dir` from its run file. `None` when the directory holds
/// none, or one that cannot be read (said in the log).
fn read_run(dir: &Path) -> Option<Resumable> {
    leviath_runtime::restore::read_for_resume(dir)
        .inspect_err(|e| {
            tracing::warn!(error = %e, "a run file could not be read; the run is not resumed");
        })
        .ok()
        .flatten()
}

/// Bind a run read back from its file and place it in the world. A binding
/// that fails is recorded on the run's file and returned.
fn resume_one(
    world: &mut PipelineWorld,
    starter: &DaemonStarter,
    run: Resumable,
) -> Result<Entity, SpawnIssues> {
    let run_id = run.spec.run_id.to_string();
    let env = starter.env_for_graph(&run.spec.graph, starter.config.current());
    let bound =
        crate::daemon::block_on::block_on(leviath_runtime::bind::bind(&run.spec, &run.code, &env));
    let bindings = bound.inspect_err(|issues| {
        let path = starter
            .runs_dir
            .join(&run_id)
            .join(leviath_core::files::RUN_FILE);
        tracing::error!(run_id = %run_id, issues = %issues, "a run could not be resumed on this machine");
        if let Err(e) = crate::daemon::starter::record_failed(&path, &run.state, issues) {
            tracing::warn!(run_id = %run_id, error = %e, "could not record why the run did not resume");
        }
    })?;
    crate::daemon::starter::store_blobs(
        starter.blob_store.as_ref(),
        &run_id,
        &run.blobs,
        &run.state.context,
    );
    starter.mcp_pool.lease_servers(
        &crate::daemon::starter::mcp_configs(&run.spec.graph),
        &run_id,
    );
    starter.hub.continue_count(&run_id, run.asked);
    Ok(leviath_runtime::restore::resume(
        world.world_mut(),
        run,
        bindings,
    ))
}

/// Bring back every unfinished run under `runs_dir`: children before the runs
/// that started them, then the ones with work to do. A run that cannot be
/// bound is recorded as failed and left out. The tree of runs is linked back
/// together once every run is in the world.
pub(crate) fn resume_all(
    world: &mut PipelineWorld,
    starter: &DaemonStarter,
    runs_dir: &Path,
) -> Recovered {
    convert_old(starter, runs_dir, true);
    // A daemon that died may have left a command it ran still running, so a
    // call that was in flight is not run again: it comes back interrupted.
    let crashed = leviath_runtime::restore::begin_session(runs_dir);
    // A directory that does not read holds nothing to bring back.
    let found: Vec<Resumable> = std::fs::read_dir(runs_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| read_run(&e.path()))
        .map(|mut run| {
            if crashed {
                leviath_runtime::restore::interrupt_in_flight(&mut run.state);
            }
            run
        })
        .collect();
    starter.refresh_world(world);
    let mut placed: Vec<Placed> = Vec::new();
    for run in leviath_runtime::restore::triage(found, |r| r) {
        let spec = run.spec.clone();
        if let Ok(entity) = resume_one(world, starter, run) {
            placed.push(Placed { spec, entity });
        }
    }
    relink_tree(world, &placed);
    Recovered {
        reloaded: placed
            .into_iter()
            .map(|p| (p.spec.run_id.to_string(), world.own_agent(p.entity)))
            .collect(),
    }
}

/// Page one unloaded run back in, on demand. `None` when there is no such run
/// on disk, when it finished or failed, or when it cannot be bound here.
/// A run that was cancelled comes back paused, so resuming it carries on.
pub(crate) fn reload_run(
    world: &mut PipelineWorld,
    starter: &DaemonStarter,
    run_id: &str,
) -> Option<AgentId> {
    let dir = starter.runs_dir.join(run_id);
    convert_old(starter, &dir, false);
    let mut run = read_run(&dir)?;
    match &run.state.status {
        RunStatus::Complete | RunStatus::Error(_) => return None,
        RunStatus::Cancelled => {
            run.state.status = RunStatus::Paused;
            run.state.phase = leviath_runtime::state::PipelinePhase::Paused;
        }
        _ => {}
    }
    starter.refresh_world(world);
    let entity = resume_one(world, starter, run).ok()?;
    Some(world.own_agent(entity))
}

/// A run brought back, with the spec it was placed from.
struct Placed {
    spec: std::sync::Arc<leviath_runtime::spec::run_spec::RunSpec>,
    entity: Entity,
}

/// Link the runs brought back into the tree they were in: each child to the
/// run that started it, and each parent to the children it recorded. A link
/// whose other end did not come back is left out.
fn relink_tree(world: &mut PipelineWorld, placed: &[Placed]) {
    use leviath_runtime::components::{AgentState, ParentRef, SubAgentChildren};
    let by_run_id: std::collections::HashMap<&str, Entity> = placed
        .iter()
        .map(|p| (p.spec.run_id.as_str(), p.entity))
        .collect();
    let w = world.world_mut();
    for Placed { spec, entity } in placed {
        if let Some(parent) = &spec.placement.parent {
            match by_run_id.get(parent.as_str()) {
                Some(&parent_entity) => {
                    w.entity_mut(*entity).insert(ParentRef {
                        parent_entity,
                        parent_agent_id: parent.to_string(),
                        depth: usize::from(spec.placement.depth),
                    });
                }
                None => {
                    // Formatted outside the macro, so the text is made
                    // whether or not a subscriber reads the fields.
                    let (child, parent) = (spec.run_id.to_string(), parent.to_string());
                    tracing::warn!(
                        run_id = %child, parent = %parent,
                        "parent run did not come back; leaving the child unlinked"
                    );
                }
            }
        }
        let recorded = w
            .get::<AgentState>(*entity)
            .map(|s| s.spawned_children_ids.clone())
            .unwrap_or_default();
        let children: Vec<Entity> = recorded
            .iter()
            .filter_map(|id| by_run_id.get(id.as_str()).copied())
            .collect();
        if !children.is_empty() {
            w.entity_mut(*entity).insert(SubAgentChildren {
                children,
                max_child_depth: usize::from(spec.launch.max_depth),
            });
        }
    }
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "recovery_resume_tests.rs"]
mod resume_tests;
