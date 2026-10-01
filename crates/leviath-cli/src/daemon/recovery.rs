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
//! twice), a choice of edge is asked again, and a fan-out picks its workers
//! back up. A run directory in the older many-file layout is converted to a
//! run file first, when this build carries the converter.

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

/// A run read back from its directory, with what binding it does not need
/// but resuming it does.
struct Found {
    run: Resumable,
    /// The files the run was given or made, by digest.
    blobs: Vec<(leviath_runtime::spec::names::Digest, Vec<u8>)>,
    /// How many questions the run had put to a person, answered or not.
    asked: u64,
}

/// Convert every run directory under `runs_dir` that is still in the older
/// many-file layout into a run file, logging what each conversion had to fill
/// in. A directory that cannot be converted is left as it is and said so.
pub(crate) fn convert_old_runs(runs_dir: &Path, agents_dir: Option<&Path>) {
    let Ok(entries) = std::fs::read_dir(runs_dir) else {
        return;
    };
    for dir in entries.flatten().map(|e| e.path()) {
        convert_old_run(&dir, agents_dir);
    }
}

/// Convert the run in `dir` into a run file when it is in the older layout.
#[cfg(feature = "legacy-runs")]
fn convert_old_run(dir: &Path, agents_dir: Option<&Path>) {
    if !leviath_legacy_runs::is_legacy(dir) {
        return;
    }
    let env = leviath_legacy_runs::ConvertEnv {
        agents_dir: agents_dir.map(Path::to_path_buf),
    };
    match leviath_legacy_runs::convert(dir, &env) {
        Ok(report) => {
            tracing::info!(
                run_id = %report.run_id,
                deltas = report.deltas,
                "converted an old run directory into a run file"
            );
            for filled in &report.defaulted {
                tracing::info!(run_id = %report.run_id, "converted from the old layout: {filled}");
            }
            for note in &report.notes {
                tracing::info!(run_id = %report.run_id, "converted from the old layout: {note}");
            }
        }
        Err(e) => {
            let shown = dir.display();
            tracing::warn!(dir = %shown, error = %e, "an old run directory could not be converted");
        }
    }
}

/// Without the converter, a run in the older layout is only reported.
#[cfg(not(feature = "legacy-runs"))]
fn convert_old_run(dir: &Path, _agents_dir: Option<&Path>) {
    let old = dir.join(leviath_core::files::META_FILE).is_file()
        && !dir.join(leviath_core::files::RUN_FILE).is_file();
    if old {
        let shown = dir.display();
        tracing::warn!(dir = %shown, "an old run directory, and this build cannot convert it");
    }
}

/// Read the run in `dir` from its run file. `None` when the directory holds
/// none, or one that cannot be read (said in the log).
fn read_run(dir: &Path) -> Option<Found> {
    let path = dir.join(leviath_core::files::RUN_FILE);
    if !path.is_file() {
        return None;
    }
    let read = leviath_runtime::runfile::RunFileReader::open(&path).and_then(|reader| {
        let state = reader.latest_state()?;
        let code = reader.code_files()?;
        let blobs = reader
            .blob_digests()
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .map(|d| Ok((d.clone(), reader.blob(&d)?.unwrap_or_default())))
            .collect::<Result<Vec<_>, leviath_runtime::runfile::RunFileError>>()?;
        let answered = reader
            .deltas(1, reader.last_seq())?
            .iter()
            .flat_map(|d| &d.events)
            .filter(|e| matches!(e, leviath_runtime::state::RunEvent::Answered { .. }))
            .count();
        let open = state.interactions.len() + state.pending.as_ref().map_or(0, |b| b.calls.len());
        Ok(Found {
            asked: u64::try_from(answered + open).unwrap_or(u64::MAX),
            run: Resumable {
                spec: std::sync::Arc::new(reader.spec().clone()),
                state,
                code,
            },
            blobs,
        })
    });
    match read {
        Ok(found) => Some(found),
        Err(e) => {
            tracing::warn!(error = %e, "a run file could not be read; the run is not resumed");
            None
        }
    }
}

/// Bind a run read back from its file and place it in the world. A binding
/// that fails is recorded on the run's file and returned.
fn resume_one(
    world: &mut PipelineWorld,
    starter: &DaemonStarter,
    found: Found,
) -> Result<Entity, SpawnIssues> {
    let Found { run, blobs, asked } = found;
    let run_id = run.spec.run_id.to_string();
    let env = starter.env_for_graph(&run.spec.graph, starter.config.current());
    let bound =
        crate::daemon::block_on::block_on(leviath_runtime::bind::bind(&run.spec, &run.code, &env));
    let bindings = match bound {
        Ok(bindings) => bindings,
        Err(issues) => {
            let path = starter
                .runs_dir
                .join(&run_id)
                .join(leviath_core::files::RUN_FILE);
            tracing::error!(run_id = %run_id, issues = %issues, "a run could not be resumed on this machine");
            if let Err(e) = crate::daemon::starter::record_failed(&path, &run.state, &issues) {
                tracing::warn!(run_id = %run_id, error = %e, "could not record why the run did not resume");
            }
            return Err(issues);
        }
    };
    crate::daemon::starter::store_blobs(
        starter.blob_store.as_ref(),
        &run_id,
        blobs.iter().map(|(d, b)| (d, b)),
        &run.state.context,
    );
    starter.mcp_pool.lease_servers(
        &crate::daemon::starter::mcp_configs(&run.spec.graph),
        &run_id,
    );
    starter.hub.continue_count(&run_id, asked);
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
    convert_old_runs(runs_dir, starter.agents_dir.as_deref());
    let Ok(entries) = std::fs::read_dir(runs_dir) else {
        return Recovered::default();
    };
    let found: Vec<Found> = entries
        .flatten()
        .filter_map(|e| read_run(&e.path()))
        .collect();
    starter.refresh_world(world);
    let mut placed: Vec<(String, Entity)> = Vec::new();
    for found in leviath_runtime::restore::triage(found, |f| &f.run) {
        let run_id = found.run.spec.run_id.to_string();
        if let Ok(entity) = resume_one(world, starter, found) {
            placed.push((run_id, entity));
        }
    }
    relink_tree(world, &placed);
    Recovered {
        reloaded: placed
            .into_iter()
            .map(|(run_id, entity)| (run_id, world.own_agent(entity)))
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
    convert_old_run(&dir, starter.agents_dir.as_deref());
    let mut found = read_run(&dir)?;
    match &found.run.state.status {
        RunStatus::Complete | RunStatus::Error(_) => return None,
        RunStatus::Cancelled => {
            found.run.state.status = RunStatus::Paused;
            found.run.state.phase = leviath_runtime::state::PipelinePhase::Paused;
        }
        _ => {}
    }
    starter.refresh_world(world);
    let entity = resume_one(world, starter, found).ok()?;
    Some(world.own_agent(entity))
}

/// Link the runs brought back into the tree they were in: each child to the
/// run that started it, and each parent to the children it recorded. A link
/// whose other end did not come back is left out.
fn relink_tree(world: &mut PipelineWorld, placed: &[(String, Entity)]) {
    use leviath_runtime::components::{AgentState, ParentRef, SubAgentChildren};
    use leviath_runtime::insert::RunSpecC;
    let by_run_id: std::collections::HashMap<&str, Entity> =
        placed.iter().map(|(id, e)| (id.as_str(), *e)).collect();
    let w = world.world_mut();
    for (run_id, entity) in placed {
        let Some(spec) = w.get::<RunSpecC>(*entity).map(|s| s.0.clone()) else {
            continue;
        };
        if let Some(parent) = &spec.placement.parent {
            match by_run_id.get(parent.as_str()) {
                Some(&parent_entity) => {
                    w.entity_mut(*entity).insert(ParentRef {
                        parent_entity,
                        parent_agent_id: parent.to_string(),
                        depth: usize::from(spec.placement.depth),
                    });
                }
                None => tracing::warn!(
                    run_id = %run_id, parent = %parent,
                    "parent run did not come back; leaving the child unlinked"
                ),
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
