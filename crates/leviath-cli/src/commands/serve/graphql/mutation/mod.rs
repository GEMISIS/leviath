//! The write side of the schema.
//!
//! Only the daemon changes a run, so a mutation here validates, asks the
//! daemon through the service layer, and reads the run back. Returning the run
//! is the point: a client does not have to guess whether the act landed, and
//! it does not need a second request to find out.
//!
//! One file per concern: [`runs`] for the acts a run goes through and the wait
//! for one to show in its record, [`blueprints`] for installing and removing
//! one, [`interactions`] for answering a pending ask, and [`exports`] for the
//! bulk export job. `RunMutation` itself stays one `#[Object] impl` with one
//! field per method, for the same reason `Query` does: this schema's field
//! order does not sort into those four groups, so `MergedObject` cannot
//! reproduce it, and each method here is a one-line delegation into its
//! group's module instead. `Mutation` itself is still the merge of
//! `RunMutation` and the admin surface, exactly as it was.

use async_graphql::{Context, Object};

use super::inputs::RegionInput;
use super::scalars::Timestamp;
use super::types::blueprint::Blueprint;
use interactions::AnswerInteractionInput;
use runs::{DeletePayload, RunPayload, SpawnRunInput};

pub(crate) mod blueprints;
pub(crate) mod exports;
pub(crate) mod interactions;
pub(crate) mod runs;

use interactions::InteractionPayload;
// Re-exported for the tests below, which build these inputs directly rather
// than through a query document.
#[cfg(test)]
use interactions::{AnswerApprovalInput, AnswerChoiceInput, AnswerTextInput, ApprovalScope};
#[cfg(test)]
use runs::{MetadataEntryInput, RegionSeedInput};
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
        #[graphql(desc = "The run to pause.")] run_id: String,
    ) -> async_graphql::Result<RunPayload> {
        runs::pause_run(ctx, run_id).await
    }

    /// Resume a paused run.
    ///
    /// Read `run.status`: `RUNNING` means it is moving again.
    async fn resume_run(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The run to resume.")] run_id: String,
    ) -> async_graphql::Result<RunPayload> {
        runs::resume_run(ctx, run_id).await
    }

    /// Start a run.
    ///
    /// Answers with the run itself, so a client renders the new row without a
    /// second request. `warnings` names checks the blueprint declared that this
    /// request's own output shape retires.
    ///
    /// The refusals are the server's, not the daemon's: a workdir outside
    /// `--workdir-root`, an unattended run on a `--no-remote-yolo` server, or a
    /// callback URL the outbound policy will not allow, each answer `FORBIDDEN`.
    async fn spawn_run(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Everything about the new run.")] input: SpawnRunInput,
    ) -> async_graphql::Result<RunPayload> {
        runs::spawn_run(ctx, input).await
    }

    /// Send a message to a run that is going.
    ///
    /// Whether it lands is the daemon's call: a stage that declared
    /// `accepts_messages = false`, or a finished run, does not take one, and the
    /// refusal says that rather than claiming the run does not exist.
    async fn send_message(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The run to message.")] run_id: String,
        #[graphql(desc = "What to say to it.")] message: String,
        #[graphql(desc = "Deliver into this context region instead of the default one.")]
        target_region: Option<RegionInput>,
    ) -> async_graphql::Result<RunPayload> {
        runs::send_message(ctx, run_id, message, target_region).await
    }

    /// Install a blueprint.
    ///
    /// A name that is already installed is a `CONFLICT`: replacing somebody's
    /// blueprint is what `updateBlueprint` is for, and doing it silently here
    /// is how a blueprint disappears without anybody asking for it.
    async fn create_blueprint(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The name to install it under.")] name: String,
        #[graphql(desc = "The manifest text.")] manifest: String,
    ) -> async_graphql::Result<Blueprint> {
        blueprints::create_blueprint(ctx, name, manifest).await
    }

    /// Replace an installed blueprint.
    ///
    /// The name is the key and does not change. Runs already spawned keep their
    /// own snapshot of what they executed, so this never rewrites history.
    async fn update_blueprint(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which installed blueprint to replace.")] name: String,
        #[graphql(desc = "The replacement manifest text.")] manifest: String,
    ) -> async_graphql::Result<Blueprint> {
        blueprints::update_blueprint(ctx, name, manifest).await
    }

    /// Uninstall a blueprint.
    ///
    /// Runs that used it keep their own copy of the manifest, so their history
    /// is unaffected: `run.blueprint` still answers.
    async fn delete_blueprint(
        &self,
        #[graphql(desc = "The installed blueprint to remove.")] name: String,
    ) -> async_graphql::Result<bool> {
        blueprints::delete_blueprint(name).await
    }

    /// Export the run store to a file, and hand back the job.
    ///
    /// Paging ten thousand runs through a connection is two hundred requests,
    /// and a client that wants everything wants it once. This returns
    /// immediately; poll `bulkExport(id:)` and fetch `downloadUrl` when it is
    /// complete.
    ///
    /// The filter is the run listing's own, so a client builds the predicate
    /// once and uses it for both. `fields` narrows each row, and an unknown name
    /// is refused rather than dropped: a column quietly missing from an export
    /// is discovered downstream, by somebody else.
    async fn bulk_export_runs(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Which runs to export. Omitted means all of them.")] filter: Option<
            super::run_filter::RunFilter,
        >,
        #[graphql(desc = "Which top-level run fields to keep in each row.")] fields: Option<
            Vec<String>,
        >,
    ) -> async_graphql::Result<super::query::BulkExport> {
        exports::bulk_export_runs(ctx, filter, fields).await
    }

    /// Delete run records.
    ///
    /// Takes exactly one of `ids` or `before`. Neither is a client that failed
    /// to build its query, and both at once is two predicates for one act, so
    /// each is refused rather than resolved one way.
    ///
    /// Deleting a run takes its sub-agents with it: their records only mean
    /// anything under the run that started them. A live run is skipped rather
    /// than removed, and deleting a record is not editing a run, so a finished
    /// one is fair game.
    async fn delete_runs(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Exactly these runs.")] ids: Option<Vec<String>>,
        #[graphql(desc = "Every finished run last touched before this second.")] before: Option<
            Timestamp,
        >,
        #[graphql(
            desc = "Delete a run whose record cannot be read, which cannot be shown to be finished.",
            default = false
        )]
        force: bool,
    ) -> async_graphql::Result<DeletePayload> {
        runs::delete_runs(ctx, ids, before, force).await
    }

    /// Answer a pending ask.
    ///
    /// The first answer wins. A second answer to the same request is not an
    /// error on the client's part: two people clicking one prompt is ordinary,
    /// and it reads as `accepted: false` rather than as a failure.
    async fn answer_interaction(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "Exactly one answer, of the kind the request takes.")]
        input: AnswerInteractionInput,
    ) -> async_graphql::Result<InteractionPayload> {
        interactions::answer_interaction(ctx, input).await
    }

    /// Cancel a run, and its sub-agents with it.
    ///
    /// Read `run.status`: `CANCELLED` means the cancel landed. A run that had
    /// already finished is a `CONFLICT`, which tells a client the difference
    /// between "you stopped it" and "it was over before you asked".
    async fn cancel_run(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The run to cancel.")] run_id: String,
    ) -> async_graphql::Result<RunPayload> {
        runs::cancel_run(ctx, run_id).await
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

/// The whole write side: the acts on runs and blueprints, plus the ones
/// `--allow-admin` opens.
///
/// Merged rather than nested, so `mutation { addMcpServer(...) }` reads the
/// same as every other mutation to a client that is allowed to use it, and does
/// not exist to a client that is not.
#[derive(async_graphql::MergedObject, Default)]
pub(crate) struct Mutation(RunMutation, super::admin::AdminMutation);
