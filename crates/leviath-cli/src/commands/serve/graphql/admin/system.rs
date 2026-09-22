//! The `startUpdate`, `checkMachine` and `createDirectory` fields: the acts
//! that touch the machine itself rather than its configuration.

use async_graphql::{Context, InputObject, SimpleObject};

use super::super::super::core::error::ServeError;
use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::types::machine::{Directory, DoctorReport};
use super::super::types::update::{UpdateJob, UpdateStep};

/// What the live diagnostics found.
#[derive(Debug, SimpleObject)]
pub(crate) struct CheckMachineResult {
    /// The report, in the same shape the `doctor` field answers with, which is
    /// what lets a client render one view for both.
    pub(crate) report: DoctorReport,
}

/// Where to make a directory, and what to call it.
#[derive(Debug, InputObject)]
pub(crate) struct CreateDirectoryRequest {
    /// The existing directory to make it in, absolute.
    pub(crate) parent_path: String,
    /// One directory name, not a path.
    pub(crate) name: String,
}

/// The directory that was made.
#[derive(Debug, SimpleObject)]
pub(crate) struct CreateDirectoryResult {
    /// The new directory, listed, so a picker can move into it without a
    /// second request.
    pub(crate) directory: Directory,
}

/// What an update should do.
#[derive(Debug, InputObject)]
pub(crate) struct StartUpdateRequest {
    /// The steps to carry out. Left out means all of them, which is what a
    /// person clicking "update" means.
    pub(crate) steps: Option<Vec<UpdateStep>>,
}

/// The update that was started.
#[derive(Debug, SimpleObject)]
pub(crate) struct StartUpdateResult {
    /// The job. Poll `updateJob(id:)`, or watch the live frames.
    pub(crate) job: UpdateJob,
}

/// Run the diagnostics that reach the network.
///
/// The plain `doctor` field answers from the config alone. This one asks a
/// provider whether a key works and the daemon whether it is there, which
/// costs a few seconds and is why it is a mutation rather than a field: it is
/// an act with a cost, and one runs at a time.
pub(crate) async fn check_machine(ctx: &Context<'_>) -> async_graphql::Result<CheckMachineResult> {
    let state = ctx.data_unchecked::<AppState>();
    let checks = super::super::super::doctor::live_checks(state)
        .await
        .gql()?;
    Ok(CheckMachineResult {
        report: super::super::query::live_doctor_report(checks),
    })
}

/// Make one directory, so a picker can offer "New Folder" rather than one
/// that refuses.
///
/// The three refusals are told apart on purpose: a path outside
/// `--workdir-root`, a parent that is not there, and a name already taken are
/// three different things to show somebody.
pub(crate) async fn create_directory(
    ctx: &Context<'_>,
    request: CreateDirectoryRequest,
) -> async_graphql::Result<CreateDirectoryResult> {
    let state = ctx.data_unchecked::<AppState>();
    let made = super::super::super::fs::made(state, &request.parent_path, &request.name).gql()?;
    Ok(CreateDirectoryResult {
        directory: Directory {
            path: made.path,
            // The directory it was made in, always: the only path the listing
            // reports no parent for is the fence itself, and the fence existed
            // before this call.
            parent: Some(made.parent),
            home: super::super::super::fs::picker_home(),
            cwd: super::super::super::fs::picker_cwd()
                .to_string_lossy()
                .into_owned(),
            // Described rather than listed: a directory that has just been
            // created is empty, and a second read of it could only say so
            // again.
            entries: Vec::new(),
        },
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
    request: StartUpdateRequest,
) -> async_graphql::Result<StartUpdateResult> {
    /// Every step, which is what a request that names none asks for.
    const EVERY_STEP: [UpdateStep; 4] = [
        UpdateStep::Binary,
        UpdateStep::Blueprints,
        UpdateStep::Keys,
        UpdateStep::Migrations,
    ];

    let state = ctx.data_unchecked::<AppState>();
    let steps = request.steps.unwrap_or_else(|| EVERY_STEP.to_vec());
    // The REST route spells the blueprints step `agents`, and the record the
    // job writes carries that word, so the request keeps it while the
    // argument reads in the vocabulary the rest of this schema uses.
    let apply = super::super::super::update_job::ApplyRequest {
        binary: steps.contains(&UpdateStep::Binary),
        agents: steps.contains(&UpdateStep::Blueprints),
        keys: steps.contains(&UpdateStep::Keys),
        migrations: steps.contains(&UpdateStep::Migrations),
    };
    // The record the registry wrote, rather than an id read back from it:
    // what a client sees now is the same record `updateJob` will answer
    // with in a moment, and there is no absent case to invent an answer for.
    let job = state
        .update_jobs
        .spawn(apply, &state.event_tx)
        .map_err(|running| ServeError::Conflict(format!("update {running} is already running")))
        .gql()?;
    Ok(StartUpdateResult {
        job: UpdateJob::from(job),
    })
}
