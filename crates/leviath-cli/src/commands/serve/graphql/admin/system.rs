//! The `startUpdate`, `runDoctorLive` and `makeDirectory` fields: the acts
//! that touch the machine itself rather than its configuration.

use async_graphql::Context;

use super::super::super::core::error::ServeError;
use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::types::machine::DoctorReport;
use super::super::types::update::UpdateJob;

/// A directory that was made.
#[derive(Debug, async_graphql::SimpleObject)]
pub(crate) struct MadeDirectory {
    /// The new directory.
    pub(crate) path: String,
    /// The directory it was made in.
    pub(crate) parent: String,
}

/// Run the diagnostics that reach the network.
///
/// The plain `doctor` field answers from the config alone. This one asks a
/// provider whether a key works and the daemon whether it is there, which
/// costs a few seconds and is why it is a mutation rather than a field: it is
/// an act with a cost, and one runs at a time.
pub(crate) async fn run_doctor_live(ctx: &Context<'_>) -> async_graphql::Result<DoctorReport> {
    let state = ctx.data_unchecked::<AppState>();
    let checks = super::super::super::doctor::live_checks(state)
        .await
        .gql()?;
    Ok(super::super::query::doctor_report(checks))
}

/// Make one directory, so a picker can offer "New Folder" rather than one
/// that refuses.
///
/// The three refusals are told apart on purpose: a path outside
/// `--workdir-root`, a parent that is not there, and a name already taken are
/// three different things to show somebody.
pub(crate) async fn make_directory(
    ctx: &Context<'_>,
    path: String,
    name: String,
) -> async_graphql::Result<MadeDirectory> {
    let state = ctx.data_unchecked::<AppState>();
    let made = super::super::super::fs::made(state, &path, &name).gql()?;
    Ok(MadeDirectory {
        path: made.path,
        parent: made.parent,
    })
}

/// Start a self-update, and hand back the job.
///
/// Answers before the work is done, because the work is a download and an
/// install: a request held open for a package manager is a console showing a
/// spinner it made up. Poll `updateJob(id:)`, or watch the live frames. One
/// update at a time: two package-manager upgrades of the same binary racing
/// each other is not a state worth debugging.
pub(crate) async fn start_update(
    ctx: &Context<'_>,
    binary: bool,
    blueprints: bool,
    keys: bool,
    migrations: bool,
) -> async_graphql::Result<UpdateJob> {
    let state = ctx.data_unchecked::<AppState>();
    // The REST route spells this part of the plan `agents`, and the record
    // the job writes carries that word, so the field keeps it while the
    // argument reads in the vocabulary the rest of this schema uses.
    let request = super::super::super::update_job::ApplyRequest {
        binary,
        agents: blueprints,
        keys,
        migrations,
    };
    // The record the registry wrote, rather than an id read back from it:
    // what a client sees now is the same record `updateJob` will answer
    // with in a moment, and there is no absent case to invent an answer for.
    let job = state
        .update_jobs
        .spawn(request, &state.event_tx)
        .map_err(|running| ServeError::Conflict(format!("update {running} is already running")))
        .gql()?;
    Ok(UpdateJob::from(job))
}
