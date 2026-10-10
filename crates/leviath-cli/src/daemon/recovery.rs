//! Restart recovery: the runs the daemon was running come back from their run
//! files when it starts, and a run it unloaded comes back when something asks
//! for it.
//!
//! A run's file holds everything about it: the spec it was resolved to, its
//! code and files, and the state it was last in. Bringing it back reads that,
//! binds the spec against this machine, and places it with `insert`. Nothing
//! is resolved again, so the run carries on with the models, tools, launch
//! policy and inputs it started with. A binding that fails (a provider that is
//! gone, an MCP server whose tools changed) is a refusal naming what changed.
//! The run is held, not ended: the refusal is recorded on its file, its state
//! is left as it was, and it is listed as paused until the machine is put
//! back, when a restart or `lev resume` brings it back where it stopped. A
//! model list a gateway has not answered yet does not hold a run back: the
//! run chose its models when it started.
//!
//! What the run was doing comes back with its state: a model call that was
//! out is made again, a tool batch in flight is dispatched again with the
//! results that came back carried over (a call that finished never runs
//! twice), a question put to a person or a stage checkpoint is asked again, a
//! choice of edge is asked again, and a fan-out picks its workers back up. A
//! run directory in the older many-file layout is converted to a run file
//! first, and a run file an alpha build wrote in an earlier binary layout is
//! upgraded, when this build carries the converter.
//!
//! A start reads only the runs that have not finished, found through the run
//! index, so a home with a thousand finished runs starts as fast as an empty
//! one.

use std::path::Path;
use std::sync::Arc;

use bevy_ecs::entity::Entity;
use leviath_runtime::host::{NotPlaced, PageIn, RunListEntry};
use leviath_runtime::restore::Resumable;
use leviath_runtime::spec::issues::SpawnIssues;
use leviath_runtime::state::RunStatus;
use leviath_runtime::world::{AgentId, PipelineWorld};

use crate::daemon::starter::DaemonStarter;

/// What a restart brought back: the `(run_id, entity)` pairs for the host to
/// map, and the listing rows of the runs this machine cannot take back as it
/// stands, for the host to hold.
#[derive(Default)]
pub(crate) struct Recovered {
    pub reloaded: Vec<(String, AgentId)>,
    pub held: Vec<RunListEntry>,
}

/// Convert the old run in `dir` (every one under it, with `all`), looking its
/// stages up the way a new run of its graph would be resolved here, and say
/// what it did the way an upgrade at start does.
fn convert_old(starter: &DaemonStarter, dir: &Path, all: bool) {
    let envs = |graph: &leviath_runtime::spec::graph::RunGraph| {
        starter.env_for_graph(graph, starter.config.current())
    };
    let agents = starter.agents_dir.as_deref();
    match all {
        true => {
            let board = leviath_runtime::control_socket::StartupBoard::default();
            crate::daemon::convert_old::convert_all(dir, agents, Some(&envs), &board)
                .finish(&crate::home_backup::Backup::of_runs(dir));
        }
        false => crate::daemon::convert_old::convert_one(dir, agents, Some(&envs)),
    }
}

/// Read the run in `dir` from its run file. `None` when the directory holds
/// none, or one that cannot be read (said in the log), or a run that never
/// resumes: one converted from an earlier release without its blueprint.
fn read_run(dir: &Path) -> Option<Resumable> {
    let run = leviath_runtime::restore::read_for_resume(dir)
        .inspect_err(|e| {
            tracing::warn!(error = %e, "a run file could not be read; the run is not resumed");
        })
        .ok()
        .flatten()?;
    if let Some(why) = run.spec.origin.never_resumes() {
        let id = run.spec.run_id.to_string();
        tracing::info!(run_id = %id, why = %why, "the run cannot resume: its graph is only what an old run recorded");
        return None;
    }
    Some(run)
}

/// A run read back from its file and bound to this machine, ready to place.
pub(crate) struct Bound {
    run: Resumable,
    bindings: leviath_runtime::spec::env::Bindings,
}

/// Bind a run read back from its file. A binding that fails holds the run:
/// why is recorded on its file, and its listing row returned. A cancelled run
/// being resumed (`revive`) comes back paused; one that cannot be bound stays
/// cancelled, with nothing recorded and no row.
async fn bind_one(
    starter: &DaemonStarter,
    mut run: Resumable,
    revive: bool,
) -> Result<Bound, Option<Box<RunListEntry>>> {
    let run_id = run.spec.run_id.to_string();
    let env = starter.env_for_graph(&run.spec.graph, starter.config.current());
    let bound = leviath_runtime::bind::bind(&run.spec, &run.code, &env).await;
    let bindings = match (bound, revive) {
        (Ok(bindings), _) => bindings,
        (Err(issues), true) => {
            tracing::warn!(run_id = %run_id, issues = %issues, "a cancelled run cannot be resumed on this machine as it stands");
            return Err(None);
        }
        (Err(issues), false) => {
            tracing::error!(run_id = %run_id, issues = %issues, "a run cannot be resumed on this machine as it stands; holding it");
            let path = starter
                .runs_dir
                .join(&run_id)
                .join(leviath_core::files::RUN_FILE);
            if let Err(e) = record_held(&path, &run.state, &issues) {
                tracing::warn!(run_id = %run_id, error = %e, "could not record why the run is held");
            }
            return Err(Some(Box::new(leviath_runtime::restore::held_entry(
                &run.spec, &run.state, &issues,
            ))));
        }
    };
    if revive {
        run.state.status = RunStatus::Paused;
        run.state.phase = leviath_runtime::state::PipelinePhase::Paused;
    }
    let lease = starter.mcp_pool.lease_servers(
        &crate::daemon::starter::mcp_configs(&run.spec.graph),
        &run_id,
    );
    Ok(Bound {
        run,
        bindings: bindings.with(lease),
    })
}

/// Place a bound run in the world.
fn place_one(world: &mut PipelineWorld, starter: &DaemonStarter, bound: Bound) -> Entity {
    let Bound { run, bindings } = bound;
    starter
        .hub
        .continue_count(run.spec.run_id.as_str(), run.asked);
    leviath_runtime::restore::resume(world.world_mut(), run, bindings)
}

/// Record on the run file at `path` that the run is held for `issues`, unless
/// the file already says so. Nothing else about the run changes, so it comes
/// back where it stopped once the machine is put back.
fn record_held(
    path: &Path,
    state: &leviath_runtime::state::RunState,
    issues: &SpawnIssues,
) -> Result<(), leviath_runtime::runfile::RunFileError> {
    if state.held.as_ref() == Some(issues) {
        return Ok(());
    }
    let mut writer = leviath_runtime::runfile::RunFileWriter::open(path, Default::default())?;
    let mut held = state.clone();
    held.held = Some(issues.clone());
    let events = vec![leviath_runtime::state::RunEvent::Log(format!(
        "held: this machine cannot take the run back as it stands. {issues}"
    ))];
    writer
        .record(held, chrono::Utc::now().timestamp(), events)
        .map(drop)
}

/// Bring back every unfinished run under `runs_dir`: children before the runs
/// that started them, then the ones with work to do. A run that cannot be
/// bound is held: recorded on its file and left out of the world. The tree of
/// runs is linked back together once every run is in the world.
pub(crate) fn resume_all(
    world: &mut PipelineWorld,
    starter: &DaemonStarter,
    runs_dir: &Path,
) -> Recovered {
    convert_old(starter, runs_dir, true);
    // A daemon that died may have left a command it ran still running, so a
    // call that was in flight is not run again: it comes back interrupted.
    let crashed = leviath_runtime::restore::begin_session(runs_dir);
    // Only a run that has not finished is read: the run index says which
    // those are without reading the file of every run that has.
    let found: Vec<Resumable> = crate::run_index::unfinished(runs_dir)
        .iter()
        .filter_map(|dir| read_run(dir))
        .map(|mut run| {
            if crashed {
                leviath_runtime::restore::interrupt_in_flight(&mut run.state);
            }
            run
        })
        .collect();
    starter.refresh_world(world);
    let mut placed: Vec<Placed> = Vec::new();
    let mut held = Vec::new();
    for run in leviath_runtime::restore::triage(found, |r| r) {
        let spec = run.spec.clone();
        // Before the serve loop runs, so binding here holds up nothing.
        match crate::daemon::block_on::block_on(bind_one(starter, run, false)) {
            Ok(bound) => placed.push(Placed {
                spec,
                entity: place_one(world, starter, bound),
            }),
            Err(entry) => held.extend(entry.map(|e| *e)),
        }
    }
    relink_tree(world, &placed);
    Recovered {
        reloaded: placed
            .into_iter()
            .map(|p| (p.spec.run_id.to_string(), world.own_agent(p.entity)))
            .collect(),
        held,
    }
}

/// Page one unloaded run back in, on demand, for `purpose`, against the
/// providers `config.toml` names now: read and bind it here, off the serve
/// loop, and say how to place it. A run that finished or failed stays where
/// it is, and so does one that was cancelled, unless it is being resumed: then
/// it comes back paused, so resuming it carries on. A run this machine cannot
/// take back is held, and its row says why.
pub(crate) async fn page_in(
    starter: Arc<DaemonStarter>,
    run_id: String,
    purpose: PageIn,
) -> Result<leviath_runtime::host::PlacePage, NotPlaced> {
    let bound = bind_paged(&starter, &run_id, purpose).await?;
    Ok(Box::new(move |world: &mut PipelineWorld| {
        Ok(place_paged(world, &starter, bound))
    }))
}

/// The I/O half of [`page_in`]: read `run_id` back and bind it.
pub(crate) async fn bind_paged(
    starter: &DaemonStarter,
    run_id: &str,
    purpose: PageIn,
) -> Result<Bound, NotPlaced> {
    use leviath_runtime::components::AgentStatus;
    let dir = starter.runs_dir.join(run_id);
    convert_old(starter, &dir, false);
    let run = read_run(&dir).ok_or(NotPlaced::Missing)?;
    let revive = match (&run.state.status, purpose) {
        (RunStatus::Complete, _) => return Err(NotPlaced::Stopped(AgentStatus::Complete)),
        (RunStatus::Error(message), _) => {
            return Err(NotPlaced::Stopped(AgentStatus::Error {
                message: message.clone(),
            }));
        }
        (RunStatus::Cancelled, PageIn::Address) => {
            return Err(NotPlaced::Stopped(AgentStatus::Cancelled));
        }
        (RunStatus::Cancelled, PageIn::Resume) => true,
        _ => false,
    };
    starter.providers.refresh(&starter.config.current());
    bind_one(starter, run, revive)
        .await
        .map_err(|held| match held {
            Some(entry) => NotPlaced::Held(entry),
            None => NotPlaced::Stopped(AgentStatus::Cancelled),
        })
}

/// The world half of [`page_in`]: bring the world up to the config as it
/// stands, and place the bound run in it.
pub(crate) fn place_paged(
    world: &mut PipelineWorld,
    starter: &DaemonStarter,
    bound: Bound,
) -> AgentId {
    starter.refresh_world(world);
    let entity = place_one(world, starter, bound);
    world.own_agent(entity)
}

/// The host's reloader: each page-in runs on a task of its own, so the serve
/// loop asks for a run and goes on driving the world until the run lands.
pub(crate) fn reloader(starter: Arc<DaemonStarter>) -> leviath_runtime::host::Reloader {
    Box::new(move |run_id, purpose| {
        let job = tokio::spawn(page_in(starter.clone(), run_id.to_string(), purpose));
        // A task that died is a run that did not come back.
        Box::pin(async move { job.await.unwrap_or(Err(NotPlaced::Missing)) })
    })
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
    // A child whose run file was written but whose parent stopped before
    // recording it. When it is a fan-out worker, its parent's fan-out decides
    // whether it is the worker for its item or is cancelled, so the item
    // runs once.
    for Placed { spec, entity } in placed {
        let Some(&parent) = spec
            .placement
            .parent
            .as_ref()
            .and_then(|p| by_run_id.get(p.as_str()))
        else {
            continue;
        };
        let recorded = w.get::<AgentState>(parent).is_some_and(|s| {
            s.spawned_children_ids
                .iter()
                .any(|c| c == spec.run_id.as_str())
        });
        if !recorded {
            leviath_runtime::fanout::settle_unrecorded_worker(w, parent, *entity);
        }
    }
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "recovery_resume_tests.rs"]
mod resume_tests;

#[cfg(test)]
#[path = "recovery_secret_tests.rs"]
mod secret_tests;
