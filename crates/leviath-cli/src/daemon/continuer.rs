//! Continuing a finished run, off the serve loop: the daemon's half of
//! `leviath_runtime::continuation`.
//!
//! Once the persistence lane has written everything it held for the run, the
//! run is read back from its file, as a page-in reads one. A fork writes its
//! new run from it first. The run is reopened, the regions that reseed run
//! their seeds against the machine as it stands, and the spec is bound; the
//! world then places it and the host continues it. Nothing is resolved again:
//! the run goes on with the models, tools, launch policy and inputs it
//! started with.

use std::sync::Arc;

use leviath_runtime::continuation;
use leviath_runtime::host::{ContinueJob, ContinueTask, Continuer, PlaceContinued};
use leviath_runtime::spec::env::ResolveEnv as _;
use leviath_runtime::spec::issues::{IssueCode, SpawnIssues, SpecPath};
use leviath_runtime::world::PipelineWorld;

use crate::daemon::recovery::{convert_old, read_run};
use crate::daemon::starter::DaemonStarter;

/// A refusal at the root of the request.
fn refusal(code: IssueCode, message: impl Into<String>) -> SpawnIssues {
    continuation::refused(code, SpecPath::root(), message)
}

/// The host's continuer. The host runs each continuation on a task of its
/// own, so the serve loop goes on driving the world until it lands.
pub(crate) fn continuer(starter: Arc<DaemonStarter>) -> Continuer {
    Box::new(move |job| -> ContinueTask { Box::pin(prepare(starter.clone(), job)) })
}

/// Read the run in `job` back, fork it when asked to, reopen it, reseed and
/// bind it, and say how to place it.
pub(crate) async fn prepare(
    starter: Arc<DaemonStarter>,
    job: ContinueJob,
) -> Result<PlaceContinued, SpawnIssues> {
    let ContinueJob {
        run_id,
        request,
        written,
    } = job;
    written.await;
    let config = starter.checked_config().await;
    let dir = starter.runs_dir.join(&run_id);
    convert_old(&starter, &dir, false);
    let run = read_run(&dir)
        .map_err(|why| {
            refusal(
                IssueCode::Unavailable,
                format!("run '{run_id}' cannot be read back: {why}"),
            )
        })?
        .ok_or_else(|| {
            refusal(
                IssueCode::Missing,
                format!("no run '{run_id}' is here; `lev ps --all` lists the runs"),
            )
        })?;
    let env = starter.env_for_graph(&run.spec.graph, config.clone());
    let (dir, run, forked_from) = match request.fork {
        false => (dir, run, None),
        true => {
            // Named like the run it came from: its blueprint, else its
            // graph's title.
            let title = run
                .spec
                .origin
                .blueprint_name()
                .or(run.spec.graph.title.as_deref())
                .unwrap_or("run")
                .to_string();
            let forked = continuation::fork(&run, &dir, env.new_run_id(&title), &starter.runs_dir)?;
            let new_dir = starter.runs_dir.join(forked.spec.run_id.as_str());
            (new_dir, forked, Some(run.spec.run_id.clone()))
        }
    };
    let mut prepared = continuation::prepare(&dir, run, request.message, forked_from)?;
    prepared.reseeded = continuation::reseed(&prepared.run, &env).await;
    starter.providers.refresh(&config);
    let bindings = leviath_runtime::bind::bind(&prepared.run.spec, &prepared.run.code, &env)
        .await
        .map_err(|issues| {
            // Said in the daemon's log too: the caller may already have gone.
            tracing::warn!(run_id = %run_id, issues = %issues, "a run cannot be continued on this machine as it stands");
            issues
        })?;
    let continued = prepared.run.spec.run_id.to_string();
    let lease = starter.mcp_pool.lease_servers(
        &crate::daemon::starter::mcp_configs(&prepared.run.spec.graph),
        &continued,
    );
    let bindings = bindings.with(lease);
    Ok(Box::new(move |world: &mut PipelineWorld| {
        starter.refresh_world(world);
        starter.hub.continue_count(&continued, prepared.run.asked);
        let entity = continuation::place(world.world_mut(), prepared, bindings);
        Ok((continued, world.own_agent(entity)))
    }))
}

#[cfg(test)]
#[path = "continuer_tests.rs"]
mod tests;
