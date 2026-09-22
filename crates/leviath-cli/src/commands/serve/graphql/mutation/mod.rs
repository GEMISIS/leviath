//! The write side of the schema.
//!
//! Only the daemon changes a run, so a mutation here validates, asks the
//! daemon through the service layer, and reads the run back. Returning the run
//! is the point: a client does not have to guess whether the act landed, and
//! it does not need a second request to find out.
//!
//! One file per concern: [`runs`] for the acts a run goes through and the wait
//! for one to show in its record, [`attachments`] for the files a spawn or a
//! message brings with it, [`blueprints`] for installing and removing one,
//! [`interactions`] for answering a pending ask, [`exports`] for the bulk
//! export job, and [`catalog`] for the one write that only refreshes a cache.
//! `RunMutation` itself stays one `#[Object] impl` with one field per method,
//! for the same reason `Query` does: this schema's field order does not sort
//! into those groups, so `MergedObject` cannot reproduce it, and each method
//! here is a one-line delegation into its group's module instead. `Mutation`
//! itself is the merge of `RunMutation` and the admin surface.

use async_graphql::{Context, Object};

use blueprints::{
    CreateBlueprintRequest, CreateBlueprintResult, DeleteBlueprintRequest, DeleteBlueprintResult,
    UpdateBlueprintRequest, UpdateBlueprintResult,
};
use catalog::{RefreshModelsRequest, RefreshModelsResult};
use exports::{StartRunExportRequest, StartRunExportResult};
use interactions::{AnswerInteractionRequest, AnswerInteractionResult};
use runs::{
    CancelRunRequest, CancelRunResult, CancelRunsRequest, CancelRunsResult, DeleteRunsRequest,
    DeleteRunsResult, PauseRunRequest, PauseRunResult, PauseRunsRequest, PauseRunsResult,
    ResumeRunRequest, ResumeRunResult, ResumeRunsRequest, ResumeRunsResult, SendMessageRequest,
    SendMessageResult, SpawnRunRequest, SpawnRunResult,
};

pub(crate) mod attachments;
pub(crate) mod blueprints;
pub(crate) mod catalog;
pub(crate) mod exports;
pub(crate) mod interactions;
pub(crate) mod runs;

// Re-exported for the tests below, which build these values directly rather
// than through a query document.
#[cfg(test)]
use runs::{has_landed, settle};

/// The acts on runs and blueprints. A finished run is immutable: these reject
/// it with `CONFLICT` rather than quietly doing nothing.
#[derive(Default)]
pub(crate) struct RunMutation;

#[Object]
impl RunMutation {
    /// Park a run.
    ///
    /// Read `run.status` on the way back: `PAUSED` means the pause landed.
    /// A finished run is a `CONFLICT`, never a silent no-op.
    async fn pause_run(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The run to pause.")] request: PauseRunRequest,
    ) -> async_graphql::Result<PauseRunResult> {
        runs::pause_run(ctx, request).await
    }

    /// Park every run a filter names.
    ///
    /// A run that had already finished is reported under `skipped` rather than
    /// failing the sweep. An empty filter names every run on this machine and
    /// is refused.
    async fn pause_runs(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The runs to pause.")] request: PauseRunsRequest,
    ) -> async_graphql::Result<PauseRunsResult> {
        runs::pause_runs(ctx, request).await
    }

    /// Resume a paused run.
    ///
    /// Read `run.status`: `RUNNING` means it is moving again.
    async fn resume_run(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The run to resume.")] request: ResumeRunRequest,
    ) -> async_graphql::Result<ResumeRunResult> {
        runs::resume_run(ctx, request).await
    }

    /// Resume every paused run a filter names.
    ///
    /// A run that is not parked is reported under `skipped`. An empty filter
    /// names every run on this machine and is refused.
    async fn resume_runs(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The runs to resume.")] request: ResumeRunsRequest,
    ) -> async_graphql::Result<ResumeRunsResult> {
        runs::resume_runs(ctx, request).await
    }

    /// Cancel a run, and its sub-agents with it.
    ///
    /// Read `run.status`: `CANCELLED` means the cancel landed. A run that had
    /// already finished is a `CONFLICT`, which tells a client the difference
    /// between "you stopped it" and "it was over before you asked".
    async fn cancel_run(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The run to cancel.")] request: CancelRunRequest,
    ) -> async_graphql::Result<CancelRunResult> {
        runs::cancel_run(ctx, request).await
    }

    /// Cancel every run a filter names, and their sub-agents with them.
    ///
    /// A run that was already over is reported under `skipped`. An empty filter
    /// names every run on this machine and is refused.
    async fn cancel_runs(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The runs to cancel.")] request: CancelRunsRequest,
    ) -> async_graphql::Result<CancelRunsResult> {
        runs::cancel_runs(ctx, request).await
    }

    /// Start a run.
    ///
    /// Answers with the run itself, so a client renders the new row without a
    /// second request. `warnings` names checks the blueprint declared that this
    /// request's own output shape retires.
    ///
    /// The refusals are the server's, not the daemon's: a workdir outside
    /// `--workdir-root`, an unattended run on a `--no-remote-yolo` server, an
    /// attachment outside the working directory, or a callback URL the outbound
    /// policy will not allow.
    async fn spawn_run(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Everything about the new run.")] request: SpawnRunRequest,
    ) -> async_graphql::Result<SpawnRunResult> {
        runs::spawn_run(ctx, request).await
    }

    /// Send a message to a run that is going.
    ///
    /// Whether it lands is the daemon's call: a stage that declared
    /// `accepts_messages = false`, or a finished run, does not take one, and the
    /// refusal says that rather than claiming the run does not exist.
    async fn send_message(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The run, the words, and anything to send with them.")]
        request: SendMessageRequest,
    ) -> async_graphql::Result<SendMessageResult> {
        runs::send_message(ctx, request).await
    }

    /// Delete run records.
    ///
    /// Deleting a run takes its sub-agents with it: their records only mean
    /// anything under the run that started them. A live run is skipped rather
    /// than removed, and deleting a record is not editing a run, so a finished
    /// one is fair game. An empty filter names every run on this machine and is
    /// refused.
    async fn delete_runs(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The runs to delete.")] request: DeleteRunsRequest,
    ) -> async_graphql::Result<DeleteRunsResult> {
        runs::delete_runs(ctx, request).await
    }

    /// Install a blueprint.
    ///
    /// A name that is already installed is a `CONFLICT`: replacing somebody's
    /// blueprint is what `updateBlueprint` is for, and doing it silently here
    /// is how a blueprint disappears without anybody asking for it.
    async fn create_blueprint(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The name to install it under, and the manifest text.")]
        request: CreateBlueprintRequest,
    ) -> async_graphql::Result<CreateBlueprintResult> {
        blueprints::create_blueprint(ctx, request).await
    }

    /// Replace an installed blueprint.
    ///
    /// The name is the key and does not change. A `digest` on the reference
    /// pins the revision being replaced, so two clients editing one blueprint
    /// cannot silently overwrite each other. Runs already spawned keep their own
    /// snapshot of what they executed, so this never rewrites history.
    async fn update_blueprint(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which blueprint to replace, and what with.")]
        request: UpdateBlueprintRequest,
    ) -> async_graphql::Result<UpdateBlueprintResult> {
        blueprints::update_blueprint(ctx, request).await
    }

    /// Uninstall a blueprint.
    ///
    /// Runs that used it keep their own copy of the manifest, so their history
    /// is unaffected: `run.blueprint` still answers. A name nothing is installed
    /// under is `NOT_FOUND`.
    async fn delete_blueprint(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The installed blueprint to remove.")] request: DeleteBlueprintRequest,
    ) -> async_graphql::Result<DeleteBlueprintResult> {
        blueprints::delete_blueprint(ctx, request).await
    }

    /// Answer a pending ask.
    ///
    /// The first answer wins. A second answer to the same request is not an
    /// error on the client's part: two people clicking one prompt is ordinary,
    /// and it reads as `ALREADY_SETTLED` rather than as a failure.
    async fn answer_interaction(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The ask, and exactly one answer of the kind it takes.")]
        request: AnswerInteractionRequest,
    ) -> async_graphql::Result<AnswerInteractionResult> {
        interactions::answer_interaction(ctx, request).await
    }

    /// Export the run store to a file, and hand back the job.
    ///
    /// Paging ten thousand runs through a connection is two hundred requests,
    /// and a client that wants everything wants it once. This returns
    /// immediately; poll `runExport(id:)` and fetch `downloadUrl` when it is
    /// complete.
    ///
    /// The filter is the run listing's own, so a client builds the predicate
    /// once and uses it for both. `fields` narrows each row, and an unknown name
    /// is refused rather than dropped: a column quietly missing from an export
    /// is discovered downstream, by somebody else.
    async fn start_run_export(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which runs to export, and which fields to keep.")]
        request: StartRunExportRequest,
    ) -> async_graphql::Result<StartRunExportResult> {
        exports::start_run_export(ctx, request).await
    }

    /// Re-read what the providers serve, and answer with the catalogue.
    ///
    /// The one write that changes nothing a run can see: `models` is a query and
    /// stays side-effect free, so going and asking is a mutation.
    async fn refresh_models(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which provider to go and ask.")] request: RefreshModelsRequest,
    ) -> async_graphql::Result<RefreshModelsResult> {
        catalog::refresh_models(ctx, request).await
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

/// The whole write side: the acts on runs and blueprints, plus the ones
/// `--allow-admin` opens.
///
/// Merged rather than nested, so an admin mutation reads the same as every
/// other mutation to a client that is allowed to use it, and does not exist to
/// a client that is not.
///
/// Every field takes one argument named `request` and answers with a result
/// carrying what it changed. Nothing inside a result is a failure: a refusal is
/// a GraphQL error, and its `extensions.code` is the thing to branch on, from
/// this vocabulary.
///
/// | `code` | `httpStatus` | Means |
/// |---|---|---|
/// | `BAD_USER_INPUT` | 400 | The request is wrong as written: a negative `maxDepth`, an empty filter on a destructive sweep, an unknown export field. Sending it again unchanged fails the same way. |
/// | `FORBIDDEN` | 403 | This server is configured to refuse it: a workdir outside `--workdir-root`, an unattended run on a `--no-remote-yolo` server, an attachment path outside the working directory. |
/// | `NOT_FOUND` | 404 | Nothing by that name, or nothing in the state the act needs: an unknown run id, a blueprint that is not installed. |
/// | `CONFLICT` | 409 | It exists and its state refuses the change: a finished run cannot be paused, a stale digest pin cannot write. |
/// | `PAYLOAD_TOO_LARGE` | 413 | An attachment is over this server's `max_upload_bytes`. |
/// | `UNPROCESSABLE` | 422 | The request is well formed and something on disk will not answer, such as a `yolo.toml` that does not parse. |
/// | `UPSTREAM` | 502 | Something this server depends on answered badly. Retrying may well work. |
/// | `DAEMON_INCOMPATIBLE` | 502 | The daemon was updated under a running `lev serve`. Restart the server. |
/// | `DAEMON_UNAVAILABLE` | 503 | The daemon could not be reached. |
/// | `INTERNAL` | 500 | Something failed that the caller did nothing wrong to cause. |
#[derive(async_graphql::MergedObject, Default)]
pub(crate) struct Mutation(RunMutation, super::admin::AdminMutation);
