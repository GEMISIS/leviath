//! The write side of the schema.
//!
//! Only the daemon changes a run, so a mutation here validates, asks the
//! daemon through the service layer, and reads the run back. Returning the run
//! is the point: a client does not have to guess whether the act landed, and
//! it does not need a second request to find out.

use async_graphql::{Context, Object, SimpleObject};

use super::super::core::error::ServeError;
use super::super::core::lifecycle::{self, Action};
use super::super::types::AppState;
use super::error::IntoGraphql;
use super::types::run::Run;
use crate::runstate;

/// What a lifecycle mutation answers with.
#[derive(SimpleObject)]
pub(crate) struct AgentPayload {
    /// The run, read back after the act, so its status is what the act made
    /// it rather than what the caller hoped.
    pub(crate) run: Run,
    /// Retired checks the mutation noticed. Empty unless something was
    /// superseded.
    pub(crate) warnings: Vec<String>,
}

/// Carry out one lifecycle action and read the run back.
async fn act_and_read(
    ctx: &Context<'_>,
    run_id: &str,
    action: Action,
) -> async_graphql::Result<AgentPayload> {
    let state = ctx.data_unchecked::<AppState>();
    lifecycle::act(state, run_id, action).await.gql()?;
    let meta = runstate::read_meta(run_id)
        .map_err(|e| {
            // The daemon accepted the act, so the run exists. A record that
            // will not read is this server's problem, not the caller's, and
            // saying so beats answering "not found" about a run that just
            // moved.
            ServeError::Internal(format!(
                "Run '{run_id}' changed, but its record would not read: {e}"
            ))
        })
        .gql()?;
    Ok(AgentPayload {
        run: Run {
            meta: std::sync::Arc::new(meta),
            now: leviath_core::duration::now_secs(),
        },
        warnings: Vec::new(),
    })
}

/// The write side. A finished run is immutable: these reject it with
/// `CONFLICT` rather than quietly doing nothing.
pub(crate) struct Mutation;

#[Object]
impl Mutation {
    /// Park a run.
    ///
    /// Read `run.status` on the way back: `PAUSED` means the pause landed.
    /// A finished run is a `CONFLICT`, never a silent no-op.
    async fn pause_agent(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The run to pause.")] run_id: String,
    ) -> async_graphql::Result<AgentPayload> {
        act_and_read(ctx, &run_id, Action::Pause).await
    }

    /// Resume a paused run.
    ///
    /// Read `run.status`: `RUNNING` means it is moving again.
    async fn resume_agent(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The run to resume.")] run_id: String,
    ) -> async_graphql::Result<AgentPayload> {
        act_and_read(ctx, &run_id, Action::Resume).await
    }

    /// Cancel a run, and its sub-agents with it.
    ///
    /// Read `run.status`: `CANCELLED` means the cancel landed. A run that had
    /// already finished is a `CONFLICT`, which tells a client the difference
    /// between "you stopped it" and "it was over before you asked".
    async fn cancel_agent(
        &self,
        ctx: &Context<'_>,
        #[graphql(desc = "The run to cancel.")] run_id: String,
    ) -> async_graphql::Result<AgentPayload> {
        act_and_read(ctx, &run_id, Action::Cancel).await
    }
}

#[cfg(test)]
#[path = "mutation_tests.rs"]
mod tests;
