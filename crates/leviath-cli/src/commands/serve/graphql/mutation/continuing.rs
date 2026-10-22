//! `continueRun`, and `spawnRun` with `source.continueFrom`: a finished run
//! taken up again with a new message, in place or as a new run forked from it.
//!
//! The daemon decides whether the run can be continued and does the work; a
//! refusal comes back typed (no such run, a run that has not finished, one
//! another request is continuing), so `continueRun` answers it as the GraphQL
//! error of that kind and `spawnRun` as a `SpawnRejectedOutput`.

use async_graphql::{Context, ID, InputObject, SimpleObject};
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};

use super::super::super::core::error::ServeError;
use super::super::super::core::spawn::{self as spawn_core, ContinueAsk, Verdict};
use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::inputs::RegionRef;
use super::super::scalars::BigInt;
use super::super::types::run::Run;
use super::attachments::AttachmentWrite;
use super::runs::{message_parts, read_meta, run_of};
use super::spawn::{SpawnRejected, SpawnRunResult, Spawned};
use super::spawn_request::{SpawnRunRequest, SpawnSourceWrite};

/// A message for a finished run.
#[derive(Debug, InputObject)]
pub(crate) struct ContinueRunRequest {
    /// The finished run: complete, failed or cancelled.
    pub(crate) run_id: ID,
    /// The message its new segment starts with.
    pub(crate) text: String,
    /// Deliver into this context region instead of the conversation.
    pub(crate) region: Option<RegionRef>,
    /// Files inside the run's working directory to send with it.
    pub(crate) attachments: Option<Vec<AttachmentWrite>>,
}

/// A run to fork from, and the message the new run starts with.
#[derive(Debug, InputObject)]
pub(crate) struct ContinueFromWrite {
    /// The finished run to fork: complete, failed or cancelled. It is only
    /// read, never changed.
    pub(crate) run_id: ID,
    /// The message the new run's work starts with.
    pub(crate) text: String,
    /// Deliver into this context region instead of the conversation.
    pub(crate) region: Option<RegionRef>,
    /// Files inside the finished run's working directory to send with it.
    pub(crate) attachments: Option<Vec<AttachmentWrite>>,
}

/// What continuing a run answers with.
#[derive(SimpleObject)]
pub(crate) struct ContinueRunResult {
    /// The run, as its record stands.
    pub(crate) run: Run,
    /// The segment that started, counted from 1. A run never continued is
    /// in segment 1.
    pub(crate) segment: i32,
    /// The event stream's number before the run was continued: every frame
    /// of the new segment is numbered above it, so `runEvents(since:)` with
    /// it hands over the segment from its `RunContinuedEvent` on.
    pub(crate) seq: BigInt,
}

/// The message `ask` carries, read: its attachments from the working
/// directory of the run `run_id`.
async fn ask_of(
    state: &AppState,
    run_id: &str,
    text: String,
    region: Option<RegionRef>,
    attachments: Option<Vec<AttachmentWrite>>,
    fork: bool,
) -> Result<ContinueAsk, ServeError> {
    Ok(ContinueAsk {
        parts: message_parts(state, run_id, attachments).await?,
        text,
        region: region.map(|r| r.name),
        fork,
    })
}

/// Continue a finished run with a new message.
pub(crate) async fn continue_run(
    ctx: &Context<'_>,
    request: ContinueRunRequest,
) -> async_graphql::Result<ContinueRunResult> {
    let state = ctx.data_unchecked::<AppState>();
    let run_id = request.run_id.to_string();
    let ask = ask_of(
        state,
        &run_id,
        request.text,
        request.region,
        request.attachments,
        false,
    )
    .await
    .gql()?;
    let continued = match spawn_core::continue_run(state, &run_id, ask).await.gql()? {
        Verdict::Accepted(continued) => continued,
        Verdict::Rejected(issues) => return Err(spawn_core::continue_refusal(&issues)).gql(),
    };
    Ok(ContinueRunResult {
        run: run_of(read_meta(&continued.run_id).await.gql()?),
        segment: i32::try_from(continued.segment).unwrap_or(i32::MAX),
        seq: BigInt(i64::try_from(continued.seq).unwrap_or(i64::MAX)),
    })
}

impl SpawnRunRequest {
    /// The fork this request asks for, with every setting it gave that a
    /// fork cannot take; or the request back, for any other source.
    pub(crate) fn fork(self) -> Result<(ContinueFromWrite, SpawnIssues), Box<Self>> {
        let refused = fork_refusals(&self);
        match self.source {
            SpawnSourceWrite::ContinueFrom(from) => Ok((from, refused)),
            source => Err(Box::new(Self { source, ..self })),
        }
    }
}

/// The settings a fork cannot take, each named where `request` set it: a
/// fork runs the finished run's own graph, models, inputs, workdir, launch
/// policy and delivery.
fn fork_refusals(request: &SpawnRunRequest) -> SpawnIssues {
    let set = [
        (
            "inputs",
            request.inputs.as_ref().is_some_and(|i| !i.is_empty()),
        ),
        (
            "attachments",
            request.attachments.as_ref().is_some_and(|a| !a.is_empty()),
        ),
        ("model", request.model.is_some()),
        ("output", request.output.is_some()),
        ("workdir", request.workdir.is_some()),
        ("launch", request.launch.is_some()),
        ("delivery", request.delivery.is_some()),
    ];
    let mut issues = SpawnIssues::new();
    for (field, _) in set.into_iter().filter(|(_, given)| *given) {
        issues.push(
            SpawnIssue::new(
                SpecPath::root().field(field),
                IssueCode::NotAllowed,
                format!("a fork runs the finished run's own settings, so it takes no `{field}`"),
            )
            .hint("leave it out; attachments for the message go in `continueFrom`"),
        );
    }
    issues
}

/// Start a new run carrying the context of the finished run `from` names,
/// continued with its message: `spawnRun` with `source.continueFrom`.
pub(crate) async fn fork(
    state: &AppState,
    from: ContinueFromWrite,
    refused: SpawnIssues,
) -> Result<SpawnRunResult, ServeError> {
    if !refused.is_empty() {
        return Ok(SpawnRunResult::Rejected(SpawnRejected::from(&refused)));
    }
    let run_id = from.run_id.to_string();
    let ask = ask_of(
        state,
        &run_id,
        from.text,
        from.region,
        from.attachments,
        true,
    )
    .await?;
    Ok(match spawn_core::continue_run(state, &run_id, ask).await? {
        Verdict::Accepted(continued) => SpawnRunResult::Spawned(Spawned {
            run_id: continued.run_id,
            warnings: SpawnIssues::new(),
            seq: continued.seq,
            deduplicated: false,
        }),
        Verdict::Rejected(issues) => SpawnRunResult::Rejected(SpawnRejected::from(&issues)),
    })
}

#[cfg(test)]
#[path = "continuing_tests.rs"]
mod tests;
