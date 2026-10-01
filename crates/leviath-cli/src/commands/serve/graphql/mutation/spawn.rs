//! `spawnRun` and `validateSpawn`: one request, started or only checked.
//!
//! Both answer with a union rather than an error, because a refused request is
//! an answer: every problem with it, each at its own path with what was
//! expected and how to fix it. A GraphQL error is left for what the caller
//! did not cause, such as a daemon that is not there.

use async_graphql::{Context, Enum, ID, Object, SimpleObject, Union};
use leviath_graphql_derive::mirror;
use leviath_runtime::spec::issues::{
    IssueCode, PathSeg, SpawnIssue as CoreIssue, SpawnIssues, SpecPath,
};

use super::super::super::core::spawn::{self as spawn_core, Verdict};
use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::types::run::Run;
use super::super::types::runfile::spec::SpawnSummary;
use super::spawn_request::{Policy, SpawnRunRequest};

/// What kind of problem a spawn issue is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum SpawnIssueCode {
    /// Something required was not given.
    Missing,
    /// A key or name that nothing declares.
    Unknown,
    /// A value of the wrong type.
    WrongType,
    /// A number outside its range, or a list or text of the wrong length.
    OutOfRange,
    /// A value without the shape its type requires: a bad name, a malformed
    /// URL, a path that leaves the working directory.
    Invalid,
    /// The same name given twice.
    Duplicate,
    /// A reference to a stage, region, edge or input that is not declared.
    Dangling,
    /// Two settings that cannot both hold.
    Conflict,
    /// Something this request is not allowed to ask for here.
    NotAllowed,
    /// This machine cannot provide what the request names: an unknown model, a
    /// blueprint that is not installed, a pin to another revision.
    Unresolvable,
    /// A resumed run names something this machine no longer has.
    Unavailable,
    /// A resumed run names something that has changed since it started.
    Changed,
}

impl From<IssueCode> for SpawnIssueCode {
    fn from(code: IssueCode) -> Self {
        match code {
            IssueCode::Missing => Self::Missing,
            IssueCode::Unknown => Self::Unknown,
            IssueCode::WrongType => Self::WrongType,
            IssueCode::OutOfRange => Self::OutOfRange,
            IssueCode::Invalid => Self::Invalid,
            IssueCode::Duplicate => Self::Duplicate,
            IssueCode::Dangling => Self::Dangling,
            IssueCode::Conflict => Self::Conflict,
            IssueCode::NotAllowed => Self::NotAllowed,
            IssueCode::Unresolvable => Self::Unresolvable,
            IssueCode::Unavailable => Self::Unavailable,
            IssueCode::Changed => Self::Changed,
        }
    }
}

/// What one step of an issue's path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum PathSegmentKind {
    /// A field of the request: `launch`, `max_depth`.
    Field,
    /// A named member of a collection: the `plan` in `stages.plan`.
    Key,
    /// A position in a list: the `2` in `items[2]`.
    Index,
}

/// One step of the path to an issue.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct PathSegment {
    /// What the step is.
    pub(crate) kind: PathSegmentKind,
    /// The field or member name, for a `FIELD` or a `KEY`.
    pub(crate) name: Option<String>,
    /// The position, for an `INDEX`.
    pub(crate) index: Option<i32>,
}

/// One problem with a spawn request.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct SpawnIssue {
    /// Where the problem is, written the way a person reads it:
    /// `inputs.items[2].id`, `source.blueprint.digest`. Paths name the
    /// runtime's request fields, which are the snake-case spelling of this
    /// schema's.
    pub(crate) path: String,
    /// The same path, one step at a time.
    pub(crate) segments: Vec<PathSegment>,
    /// What kind of problem it is.
    pub(crate) code: SpawnIssueCode,
    /// One sentence saying what is wrong.
    pub(crate) message: String,
    /// What would have been accepted, when that is one thing.
    pub(crate) expected: Option<String>,
    /// What arrived instead.
    pub(crate) got: Option<String>,
    /// How to fix it.
    pub(crate) hint: Option<String>,
    /// The valid choices, when the problem is picking one.
    pub(crate) known: Vec<String>,
}

/// One step of a path, typed.
fn segment(seg: &PathSeg) -> PathSegment {
    match seg {
        PathSeg::Field(name) => PathSegment {
            kind: PathSegmentKind::Field,
            name: Some(name.clone()),
            index: None,
        },
        PathSeg::Key(name) => PathSegment {
            kind: PathSegmentKind::Key,
            name: Some(name.clone()),
            index: None,
        },
        PathSeg::Index(at) => PathSegment {
            kind: PathSegmentKind::Index,
            name: None,
            index: Some(i32::try_from(*at).unwrap_or(i32::MAX)),
        },
    }
}

/// The path segments of `path`.
fn segments(path: &SpecPath) -> Vec<PathSegment> {
    path.0.iter().map(segment).collect()
}

impl From<&CoreIssue> for SpawnIssue {
    fn from(issue: &CoreIssue) -> Self {
        Self {
            path: issue.path.to_string(),
            segments: segments(&issue.path),
            code: SpawnIssueCode::from(issue.code),
            message: issue.message.clone(),
            expected: issue.expected.clone(),
            got: issue.got.clone(),
            hint: issue.hint.clone(),
            known: issue.known.clone(),
        }
    }
}

/// A request refused: every problem with it, at once.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct SpawnRejected {
    /// The problems, in the order they were found. Never empty.
    pub(crate) issues: Vec<SpawnIssue>,
}

impl From<&SpawnIssues> for SpawnRejected {
    fn from(issues: &SpawnIssues) -> Self {
        Self {
            issues: issues.iter().map(SpawnIssue::from).collect(),
        }
    }
}

/// A run that was started.
pub(crate) struct Spawned {
    /// Its id.
    pub(crate) run_id: String,
}

/// A run that was started: its id, and the run itself once its record is
/// written.
#[mirror(no_filter)]
#[Object]
impl Spawned {
    /// The new run's id.
    async fn run_id(&self) -> ID {
        ID(self.run_id.clone())
    }

    /// The run as its record stands. Null in the moment between the daemon
    /// starting it and writing its first record; read `runId` then, and ask
    /// for the run again.
    async fn run(&self) -> Option<Run> {
        crate::runstate::read_meta(&self.run_id)
            .ok()
            .map(|meta| Run {
                meta: std::sync::Arc::new(meta),
                now: leviath_core::duration::now_secs(),
            })
    }
}

/// What `spawnRun` answers with: the run it started, or every reason it did
/// not.
#[derive(Union)]
pub(crate) enum SpawnRunResult {
    /// The run started.
    Spawned(Spawned),
    /// The request was refused, and nothing started.
    Rejected(SpawnRejected),
}

/// What `validateSpawn` answers with: the run the request would start, in
/// brief, or every reason it would be refused.
#[derive(Union)]
pub(crate) enum ValidateSpawnResult {
    /// The request is good; this is what it would run.
    Valid(Box<SpawnSummary>),
    /// The request would be refused.
    Rejected(SpawnRejected),
}

/// What a request is read with on this server.
fn policy(state: &AppState) -> Policy {
    Policy {
        max_bytes: state.limits.request_limits.max_upload_bytes,
    }
}

/// The request read, or every issue with it. A request with values that did
/// not read is still checked the whole way, with those values left out, so
/// its other issues are listed beside them rather than after a resubmit. A
/// daemon that cannot be asked adds nothing: the issues already found are
/// the answer, and the resubmit meets the daemon again.
async fn read(
    state: &AppState,
    request: SpawnRunRequest,
) -> Result<leviath_runtime::spec::request::SpawnRequest, SpawnIssues> {
    let (request, mut issues) = request.read(&policy(state))?;
    if issues.is_empty() {
        return Ok(request);
    }
    if let Ok(Verdict::Rejected(more)) = spawn_core::validate(state, request).await {
        issues.absorb(more);
    }
    Err(issues)
}

/// Start a run, or say every reason it cannot start.
///
/// The request is read here; the service layer adds this server's own
/// refusals and asks the daemon, so the issues are the same ones REST answers
/// with.
pub(crate) async fn spawn_run(
    ctx: &Context<'_>,
    request: SpawnRunRequest,
) -> async_graphql::Result<SpawnRunResult> {
    let state = ctx.data_unchecked::<AppState>();
    let request = match read(state, request).await {
        Ok(request) => request,
        Err(issues) => return Ok(SpawnRunResult::Rejected(SpawnRejected::from(&issues))),
    };
    Ok(match spawn_core::start(state, request).await.gql()? {
        Verdict::Accepted(run_id) => SpawnRunResult::Spawned(Spawned { run_id }),
        Verdict::Rejected(issues) => SpawnRunResult::Rejected(SpawnRejected::from(&issues)),
    })
}

/// Check a request the whole way without starting anything.
pub(crate) async fn validate_spawn(
    ctx: &Context<'_>,
    request: SpawnRunRequest,
) -> async_graphql::Result<ValidateSpawnResult> {
    let state = ctx.data_unchecked::<AppState>();
    let request = match read(state, request).await {
        Ok(request) => request,
        Err(issues) => return Ok(ValidateSpawnResult::Rejected(SpawnRejected::from(&issues))),
    };
    Ok(match spawn_core::validate(state, request).await.gql()? {
        Verdict::Accepted(summary) => {
            ValidateSpawnResult::Valid(Box::new(SpawnSummary::from(&summary)))
        }
        Verdict::Rejected(issues) => ValidateSpawnResult::Rejected(SpawnRejected::from(&issues)),
    })
}

#[cfg(test)]
#[path = "spawn_tests.rs"]
mod tests;
