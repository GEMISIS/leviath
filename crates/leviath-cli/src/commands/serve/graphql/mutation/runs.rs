//! The acts a run itself goes through: `pauseRun`, `resumeRun`, `cancelRun`
//! and their bulk twins, `spawnRun`, `sendMessage` and `deleteRuns`.
//!
//! Each one answers with the run it moved, so a client renders the new state
//! without a second request. That means waiting for the act to show in the
//! run's own record, because the daemon applies it to its world before the
//! persistence lane writes it down. A sweep waits on its runs together rather
//! than one after another, so the wait is one window and not one per run.
//!
//! The bulk acts are a loop over the same service call. A sweep names runs by a
//! predicate, and one of them being finished already is no reason to refuse the
//! rest, so those come back under `skipped` with the reason rather than as an
//! error.

use async_graphql::{Context, Enum, ID, InputObject, OneofObject, SimpleObject};
use futures_util::StreamExt;
use leviath_graphql_derive::mirror;

use super::super::super::core::error::ServeError;
use super::super::super::core::lifecycle::{self, Action};
use super::super::super::core::runs as run_core;
use super::super::super::core::spawn as spawn_core;
use super::super::super::types::AppState;
use super::super::error::{IntoGraphql, graphql_error};
use super::super::filter::run_predicate;
use super::super::inputs::{BlueprintRef, KeyValueWrite, RegionRef};
use super::super::types::run::Run;
use super::super::types::run::RunFilter;
use super::attachments::{AttachmentWrite, parts_of};
use crate::runstate;

/// Seed text for one context region at spawn.
#[derive(Debug, InputObject)]
pub(crate) struct RegionSeedWrite {
    /// The region to seed.
    pub(crate) region: RegionRef,
    /// The text it starts with.
    pub(crate) text: String,
}

/// How a run answers prompts it would otherwise put to a person.
///
/// Exactly one of the two: a blanket waiver, or the named profile that says
/// which prompts are waived. Both at once is two answers to one question, and
/// the schema says so rather than the server refusing it after the fact.
#[derive(Debug, OneofObject)]
pub(crate) enum YoloWrite {
    /// Waive every prompt. Refused outright on a server started with
    /// `--no-remote-yolo`.
    Everything(bool),
    /// Waive the prompts the named profile in `yolo.toml` waives.
    ProfileName(String),
}

/// The output shape a spawn asks the run for.
#[derive(Debug, InputObject)]
pub(crate) struct OutputRequestWrite {
    /// The format label to ask for. Carried through opaquely, so a house
    /// format needs no server support.
    pub(crate) format: Option<String>,
    /// Extra instructions for the run's output stage.
    pub(crate) instructions: Option<String>,
}

/// Where a run posts its events, and what signs them.
///
/// The secret sits inside the URL's own object, so a secret with nothing to
/// sign for cannot be written down at all.
#[derive(Debug, InputObject)]
pub(crate) struct CallbackWrite {
    /// The URL the daemon POSTs this run's events to. Checked against the same
    /// outbound policy a model-supplied URL is.
    pub(crate) url: String,
    /// Shared secret for signing that webhook body. Write-only: never read
    /// back on the run.
    pub(crate) secret: Option<String>,
}

/// Everything about a new run.
#[derive(Debug, InputObject)]
pub(crate) struct SpawnRunRequest {
    /// The blueprint to start. A `digest` on it refuses the spawn where what is
    /// installed under that name is a different revision, which is how a client
    /// starts the blueprint it read rather than whatever is there now.
    pub(crate) blueprint: BlueprintRef,
    /// The initial ask.
    pub(crate) task: String,
    /// Override the blueprint's model for this run, as `provider/model` or a
    /// bare model name. Wins over every other model setting.
    pub(crate) model: Option<String>,
    /// How deep sub-agent spawning may nest for this run.
    pub(crate) max_depth: Option<i32>,
    /// Where the run's tools execute. Defaults to this server's own directory,
    /// and is refused outside `--workdir-root` when the operator set one.
    pub(crate) workdir: Option<String>,
    /// Run unattended, in one of the two ways there are to do it. Absent means
    /// the run asks a person.
    pub(crate) yolo: Option<YoloWrite>,
    /// Tools to allow without asking, for this run.
    pub(crate) allow_tools: Option<Vec<String>>,
    /// Refuse this blueprint's command seeds, which run before any approval
    /// prompt exists.
    #[graphql(default = false)]
    pub(crate) skip_seed_commands: bool,
    /// Write this run's exact requests into its journal, once per provider
    /// attempt, whatever this machine is configured to do for other runs.
    ///
    /// A captured request is the whole prompt, holding whatever the run's
    /// context held: file contents, command output, the words somebody typed.
    /// There is no size cap, and every call re-sends the window, so a captured
    /// run's journal grows by roughly the context size per attempt. Read it
    /// back on `InferenceAttempt.modelInput`.
    #[graphql(default = false)]
    pub(crate) capture_model_input: bool,
    /// Seed text for named context regions.
    pub(crate) regions: Option<Vec<RegionSeedWrite>>,
    /// Caller-supplied metadata: labels for whoever started the run, such as a
    /// ticket or a tenant. The run reads none of them, and the run search looks
    /// through them. Not a typed extension point.
    pub(crate) metadata: Option<Vec<KeyValueWrite>>,
    /// The output shape to ask the run for, instead of the blueprint's own.
    pub(crate) output: Option<OutputRequestWrite>,
    /// Where to post this run's events, and what signs them.
    pub(crate) callback: Option<CallbackWrite>,
    /// Files inside the working directory to start the run with.
    pub(crate) attachments: Option<Vec<AttachmentWrite>>,
}

/// What a spawn answers with.
#[derive(SimpleObject)]
pub(crate) struct SpawnRunResult {
    /// The run that was started.
    pub(crate) run: Run,
    /// Retired checks the blueprint declared that this request's own output
    /// shape supersedes. Empty unless something was.
    pub(crate) warnings: Vec<String>,
}

/// A message for a run that is going.
#[derive(Debug, InputObject)]
pub(crate) struct SendMessageRequest {
    /// The run to message.
    pub(crate) run_id: ID,
    /// What to say to it.
    pub(crate) text: String,
    /// Deliver into this context region instead of the default one.
    pub(crate) region: Option<RegionRef>,
    /// Files inside the run's working directory to send with it.
    pub(crate) attachments: Option<Vec<AttachmentWrite>>,
}

/// What a message answers with.
#[derive(SimpleObject)]
pub(crate) struct SendMessageResult {
    /// The run the message went to, as its record stands.
    pub(crate) run: Run,
}

/// Why a bulk act passed one run over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum SkipReason {
    /// It is still going, and a record cannot be deleted from under a live
    /// run.
    StillRunning,
    /// Its record will not read, so nothing can be shown about it. `force`
    /// deletes such a run anyway.
    RecordUnreadable,
    /// It had already finished, so the act had nothing left to do.
    AlreadyFinished,
    /// Something else, which `message` describes.
    Other,
}

/// One run a bulk act passed over, and why.
#[mirror(no_filter)]
#[derive(Debug, SimpleObject)]
pub(crate) struct Skipped {
    /// The run that stayed as it was.
    pub(crate) id: ID,
    /// Why it did, as something to branch on.
    pub(crate) reason: SkipReason,
    /// The same reason written out, for a person reading it.
    pub(crate) message: String,
}

/// The reason a service call gave, as the word a client branches on.
///
/// The service layer answers a bulk delete with a sentence per run rather than
/// a kind, because that sentence is what REST puts in its body. The two
/// sentences it writes are the two states a run can be in when a delete passes
/// it over, so they map here rather than being handed to a client to parse.
fn skip_reason(message: &str) -> SkipReason {
    if message.contains("cancel it before deleting it") {
        return SkipReason::StillRunning;
    }
    if message.contains("has no readable record") {
        return SkipReason::RecordUnreadable;
    }
    SkipReason::Other
}

/// How long to keep looking for an act in the run's record before answering
/// with what is there.
///
/// The daemon applies the act to its world before it answers, and the record on
/// disk is written by the persistence lane a tick later, so a read that happens
/// straight after the answer sees the status the run held when it was asked.
/// The window is one tick of a world that has just been woken, so this is
/// generous rather than tuned; it exists so the answer is the run as the act
/// left it, not so the caller waits.
const SETTLE_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

/// How often to look again inside [`SETTLE_WINDOW`].
const SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// Whether `status` is what `action` leaves behind.
///
/// Asked per action rather than by watching for any change, so acting on a run
/// that is already there answers at once instead of waiting out the window for
/// a change that is never coming. A resume is the odd one: what it lands on
/// depends on what the run goes back to doing, so the only thing it promises is
/// that the run is no longer parked.
pub(super) fn has_landed(action: Action, status: &leviath_core::run_meta::RunStatus) -> bool {
    use leviath_core::run_meta::RunStatus;
    match action {
        Action::Pause => matches!(status, RunStatus::Paused),
        Action::Cancel => matches!(status, RunStatus::Cancelled),
        Action::Resume => !matches!(status, RunStatus::Paused),
    }
}

/// Read the run until the act shows in its record, or the window closes.
///
/// `read` is handed in so a test can decide what the record says on each look
/// without a daemon, a disk or a real clock behind it.
pub(super) async fn settle(
    action: Action,
    deadline: std::time::Instant,
    mut read: impl FnMut() -> Result<leviath_core::run_meta::RunMeta, ServeError>,
) -> Result<leviath_core::run_meta::RunMeta, ServeError> {
    loop {
        let meta = read()?;
        if has_landed(action, &meta.status) || std::time::Instant::now() >= deadline {
            return Ok(meta);
        }
        tokio::time::sleep(SETTLE_POLL).await;
    }
}

/// The run's record, or this server's own failure for one that will not read.
///
/// The daemon accepted the act, so the run exists: saying "not found" about a
/// run that just moved would blame the caller for this server's problem.
fn read_meta(run_id: &str) -> Result<leviath_core::run_meta::RunMeta, ServeError> {
    runstate::read_meta(run_id).map_err(|e| {
        ServeError::Internal(format!(
            "Run '{run_id}' changed, but its record would not read: {e}"
        ))
    })
}

/// One run record as the object a client reads.
fn run_of(meta: leviath_core::run_meta::RunMeta) -> Run {
    Run {
        meta: std::sync::Arc::new(meta),
        now: leviath_core::duration::now_secs(),
    }
}

/// Carry out one lifecycle action against one run, and read the run back.
async fn act_and_read(
    ctx: &Context<'_>,
    run_id: &str,
    action: Action,
) -> async_graphql::Result<Run> {
    let state = ctx.data_unchecked::<AppState>();
    lifecycle::act(state, run_id, action).await.gql()?;
    let meta = settle(action, std::time::Instant::now() + SETTLE_WINDOW, || {
        read_meta(run_id)
    })
    .await
    .gql()?;
    Ok(run_of(meta))
}

/// The runs a filter names, resolved to ids.
///
/// The one place a destructive bulk act turns a predicate into a list of runs,
/// so the acts below share a filter with the `runs` listing rather than each
/// growing its own way of saying which runs it is about.
///
/// Naming ids outright says which runs to read, not which ones to act on: the
/// rest of the filter still decides, exactly as it does on the listing. A run
/// the filter excluded is not acted on because its id was written down.
///
/// The one id a named list adds on its own is a run the index has never seen.
/// A run whose record will not parse is invisible to every listing, so nothing
/// else could ever name it, and dropping it here would leave it both invisible
/// and undeletable; it comes back as `skipped` with the reason instead, which
/// is what `force` is then for.
pub(super) async fn matching_run_ids(
    state: &AppState,
    filter: RunFilter,
    act: &str,
) -> Result<Vec<String>, ServeError> {
    let named = run_predicate::named_ids(&filter)?;
    let mut ids: Vec<String> = run_predicate::selection_for(filter, act, state)
        .await?
        .iter()
        .map(|meta| meta.run_id.clone())
        .collect();
    // A set, because one filter may name the same id twice - `eq` and `in`
    // both - and an act carried out twice over one run is not what either
    // spelling asked for. The snapshot is read only where ids were named,
    // because that is the only way a run outside the walk's answer can be
    // reached at all.
    let unlisted: std::collections::BTreeSet<String> = match named {
        None => std::collections::BTreeSet::new(),
        Some(named) => {
            let snapshot = state.caches.run_index.snapshot().await;
            named
                .into_iter()
                .filter(|id| snapshot.get(id).is_none())
                .collect()
        }
    };
    ids.extend(unlisted);
    Ok(ids)
}

/// How many runs a sweep waits on at once.
///
/// A sweep is a set, and waiting each run's record out one after another would
/// cost one settle window per run: a hundred runs would be the better part of a
/// minute of waiting for reads that have nothing to do with each other. They
/// are waited on together instead, and the bound is what keeps a sweep over a
/// large set from having a file open per run at the same time.
const SETTLE_LANES: usize = 8;

/// One run's record, once the act shows in it, carried back with its id.
///
/// The id travels with the answer because the settles finish as a set and a
/// record that will not read has no run to name itself with.
async fn settled_run(
    id: String,
    action: Action,
    deadline: std::time::Instant,
) -> (String, Result<leviath_core::run_meta::RunMeta, ServeError>) {
    let found = settle(action, deadline, || read_meta(&id)).await;
    (id, found)
}

/// One lifecycle act over every run a filter names.
///
/// A run the act has nothing to do to is skipped rather than failing the sweep.
/// A daemon that cannot be reached is not a fact about one run, so it stays a
/// failure and ends the whole act.
///
/// The acts go one at a time and the waiting is shared: each run is asked in
/// turn, so a run that finished while the sweep was working its way towards it
/// is refused by the service call rather than acted on, and the records are
/// then read back together against one deadline, so the answer carries each run
/// as the act left it rather than as it stood before.
async fn act_over(
    ctx: &Context<'_>,
    filter: RunFilter,
    action: Action,
    act: &str,
) -> async_graphql::Result<(Vec<Run>, Vec<Skipped>)> {
    let state = ctx.data_unchecked::<AppState>();
    let ids = matching_run_ids(state, filter, act).await.gql()?;
    let mut acted: Vec<String> = Vec::new();
    let mut skipped = Vec::new();
    for id in ids {
        match lifecycle::act(state, &id, action).await {
            Ok(()) => acted.push(id),
            // It was over before the sweep reached it, or it ended while the
            // act was on its way, which is the ordinary outcome of acting on a
            // set rather than on one run.
            Err(failure @ ServeError::Conflict(_)) => skipped.push(Skipped {
                id: ID::from(id),
                reason: SkipReason::AlreadyFinished,
                message: failure.to_string(),
            }),
            // The daemon does not have it: it finished and was reaped, or the
            // run index is a moment behind the store.
            Err(failure @ ServeError::NotFound(_)) => skipped.push(Skipped {
                id: ID::from(id),
                reason: SkipReason::Other,
                message: failure.to_string(),
            }),
            Err(other) => return Err(graphql_error(&other)),
        }
    }
    let deadline = std::time::Instant::now() + SETTLE_WINDOW;
    let mut settling = futures_util::stream::iter(acted)
        .map(|id| settled_run(id, action, deadline))
        .buffered(SETTLE_LANES);
    let mut moved = Vec::new();
    while let Some((id, found)) = settling.next().await {
        match found {
            Ok(meta) => moved.push(run_of(meta)),
            Err(failure) => skipped.push(Skipped {
                id: ID::from(id),
                reason: SkipReason::RecordUnreadable,
                message: failure.to_string(),
            }),
        }
    }
    Ok((moved, skipped))
}

/// Write the request and result types for one lifecycle act, single and bulk.
///
/// The four shapes are identical across `pause`, `resume` and `cancel`, and
/// GraphQL needs a distinctly named request and result per field, so they are
/// written once here and the names are the macro's arguments. A generic type
/// would need a central list of every concrete instantiation before
/// `InputObject` would register them, which is one more place to keep in step
/// than this is.
macro_rules! lifecycle_types {
    ($word:literal, $one_request:ident, $one_result:ident, $many_request:ident, $many_result:ident) => {
        #[doc = concat!("Which run to ", $word, ".")]
        #[derive(Debug, InputObject)]
        pub(crate) struct $one_request {
            /// The run.
            pub(crate) id: ID,
        }

        #[doc = concat!("What a ", $word, " answers with.")]
        #[derive(SimpleObject)]
        pub(crate) struct $one_result {
            /// The run as the act left it.
            ///
            /// The daemon applies the act to its world before it answers, and
            /// writes the record a moment later, so this waits for the act to
            /// show there before answering. A run slow to write its record is
            /// answered with the record as it stands rather than held any
            /// longer, so a status that does not yet show the act means the
            /// act is still on its way.
            pub(crate) run: Run,
            /// Advisories the act raised. Empty unless there were any.
            pub(crate) warnings: Vec<String>,
        }

        #[doc = concat!("Which runs to ", $word, ".")]
        #[derive(Debug, InputObject)]
        pub(crate) struct $many_request {
            #[doc = concat!("The runs to ", $word, ". An empty filter names every run on")]
            /// this machine and is refused: a sweep over everything is almost
            /// always a query that was built wrong.
            pub(crate) filter: RunFilter,
        }

        #[doc = concat!("What a bulk ", $word, " answers with.")]
        #[derive(SimpleObject)]
        pub(crate) struct $many_result {
            /// The runs that moved, each as its record stands afterwards.
            pub(crate) runs: Vec<Run>,
            /// The ones the act passed over, each with the reason.
            pub(crate) skipped: Vec<Skipped>,
        }
    };
}

lifecycle_types!(
    "pause",
    PauseRunRequest,
    PauseRunResult,
    PauseRunsRequest,
    PauseRunsResult
);
lifecycle_types!(
    "resume",
    ResumeRunRequest,
    ResumeRunResult,
    ResumeRunsRequest,
    ResumeRunsResult
);
lifecycle_types!(
    "cancel",
    CancelRunRequest,
    CancelRunResult,
    CancelRunsRequest,
    CancelRunsResult
);

/// Park a run.
///
/// Read `run.status` on the way back: `PAUSED` means the pause landed. A
/// finished run is a `CONFLICT`, never a silent no-op.
pub(crate) async fn pause_run(
    ctx: &Context<'_>,
    request: PauseRunRequest,
) -> async_graphql::Result<PauseRunResult> {
    Ok(PauseRunResult {
        run: act_and_read(ctx, request.id.as_str(), Action::Pause).await?,
        warnings: Vec::new(),
    })
}

/// Park every run a filter names.
///
/// A run that had already finished is reported under `skipped` rather than
/// failing the sweep, because one finished run is no reason to leave the rest
/// going.
pub(crate) async fn pause_runs(
    ctx: &Context<'_>,
    request: PauseRunsRequest,
) -> async_graphql::Result<PauseRunsResult> {
    let (runs, skipped) = act_over(ctx, request.filter, Action::Pause, "pause").await?;
    Ok(PauseRunsResult { runs, skipped })
}

/// Resume a paused run.
///
/// Read `run.status`: `RUNNING` means it is moving again.
pub(crate) async fn resume_run(
    ctx: &Context<'_>,
    request: ResumeRunRequest,
) -> async_graphql::Result<ResumeRunResult> {
    Ok(ResumeRunResult {
        run: act_and_read(ctx, request.id.as_str(), Action::Resume).await?,
        warnings: Vec::new(),
    })
}

/// Resume every paused run a filter names.
///
/// A run that is not parked is reported under `skipped`, so a sweep over a
/// mixed set moves the ones it can.
pub(crate) async fn resume_runs(
    ctx: &Context<'_>,
    request: ResumeRunsRequest,
) -> async_graphql::Result<ResumeRunsResult> {
    let (runs, skipped) = act_over(ctx, request.filter, Action::Resume, "resume").await?;
    Ok(ResumeRunsResult { runs, skipped })
}

/// Cancel a run, and its sub-agents with it.
///
/// Read `run.status`: `CANCELLED` means the cancel landed. A run that had
/// already finished is a `CONFLICT`, which tells a client the difference
/// between "you stopped it" and "it was over before you asked".
pub(crate) async fn cancel_run(
    ctx: &Context<'_>,
    request: CancelRunRequest,
) -> async_graphql::Result<CancelRunResult> {
    Ok(CancelRunResult {
        run: act_and_read(ctx, request.id.as_str(), Action::Cancel).await?,
        warnings: Vec::new(),
    })
}

/// Cancel every run a filter names, and their sub-agents with them.
///
/// A run that was already over is reported under `skipped`, which is what makes
/// "stop everything this blueprint started" one request.
pub(crate) async fn cancel_runs(
    ctx: &Context<'_>,
    request: CancelRunsRequest,
) -> async_graphql::Result<CancelRunsResult> {
    let (runs, skipped) = act_over(ctx, request.filter, Action::Cancel, "cancel").await?;
    Ok(CancelRunsResult { runs, skipped })
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
pub(crate) async fn spawn_run(
    ctx: &Context<'_>,
    request: SpawnRunRequest,
) -> async_graphql::Result<SpawnRunResult> {
    let state = ctx.data_unchecked::<AppState>();
    let max_depth = match request.max_depth {
        None => None,
        Some(depth) => Some(
            usize::try_from(depth)
                .map_err(|_| ServeError::BadRequest("`maxDepth` cannot be negative".to_string()))
                .gql()?,
        ),
    };
    // The same default the REST route uses, resolved here because the
    // attachments are read against it before the daemon sees the request.
    let workdir = request.workdir.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .map(|dir| dir.to_string_lossy().to_string())
            .unwrap_or_default()
    });
    let parts = parts_of(
        request.attachments.unwrap_or_default(),
        std::path::Path::new(&workdir),
        state.limits.request_limits.max_upload_bytes,
    )
    .gql()?;
    let (yolo, yolo_profile) = match request.yolo {
        None => (false, None),
        Some(YoloWrite::Everything(everything)) => (everything, None),
        Some(YoloWrite::ProfileName(name)) => (false, Some(name)),
    };
    let (callback_url, callback_secret) = match request.callback {
        None => (None, None),
        Some(callback) => (Some(callback.url), callback.secret),
    };
    let blueprint = request.blueprint.installed(state).await.gql()?;
    let spawn = spawn_core::SpawnRequest {
        blueprint,
        task: request.task,
        model: request.model,
        max_depth,
        workdir: Some(workdir),
        yolo,
        yolo_profile,
        allow: request.allow_tools.unwrap_or_default(),
        no_seed_commands: request.skip_seed_commands,
        capture_model_input: request.capture_model_input,
        regions: request
            .regions
            .into_iter()
            .flatten()
            .map(|seed| (seed.region.name, seed.text))
            .collect(),
        metadata: request
            .metadata
            .into_iter()
            .flatten()
            .map(|entry| (entry.key, entry.value))
            .collect(),
        callback_url,
        callback_secret,
        output: request
            .output
            .map(|output| leviath_core::output::OutputSpec {
                format: output.format,
                instructions: output.instructions,
                ..leviath_core::output::OutputSpec::default()
            }),
    };
    let spawned = spawn_core::spawn(state, spawn, parts).await.gql()?;
    Ok(SpawnRunResult {
        run: run_of(read_meta(&spawned.run_id).gql()?),
        warnings: spawned.warnings,
    })
}

/// Send a message to a run that is going.
///
/// Whether it lands is the daemon's call: a stage that declared
/// `accepts_messages = false`, or a finished run, does not take one, and the
/// refusal says that rather than claiming the run does not exist.
pub(crate) async fn send_message(
    ctx: &Context<'_>,
    request: SendMessageRequest,
) -> async_graphql::Result<SendMessageResult> {
    let state = ctx.data_unchecked::<AppState>();
    let run_id = request.run_id.to_string();
    // Attachments are named inside the run's own working directory, so the
    // record is read first for a request that sends any.
    let listed = request.attachments.unwrap_or_default();
    let parts = match listed.is_empty() {
        true => Vec::new(),
        false => {
            let meta = read_meta(&run_id).gql()?;
            parts_of(
                listed,
                std::path::Path::new(&meta.workdir),
                state.limits.request_limits.max_upload_bytes,
            )
            .gql()?
        }
    };
    let region = request.region.map(|region| region.name);
    spawn_core::send_message(state, &run_id, request.text, region, parts)
        .await
        .gql()?;
    Ok(SendMessageResult {
        run: run_of(read_meta(&run_id).gql()?),
    })
}

/// Which runs to delete, and whether an unreadable record goes with them.
#[derive(Debug, InputObject)]
pub(crate) struct DeleteRunsRequest {
    /// The runs to delete. An empty filter names every run on this machine and
    /// is refused. "Every finished run before a date" is
    /// `updatedAt: { lt: ... }`.
    pub(crate) filter: RunFilter,
    /// Delete a run whose record cannot be read, and so cannot be shown to be
    /// finished.
    #[graphql(default = false)]
    pub(crate) force: bool,
}

/// What a delete removed, and what it left.
///
/// Partial success is the normal outcome rather than an edge case: a sweep
/// names runs by a predicate, and one of them being live is no reason to refuse
/// the rest. Read `skipped` when the list does not empty.
#[derive(Debug, SimpleObject)]
pub(crate) struct DeleteRunsResult {
    /// The runs that were removed, sub-agents included.
    pub(crate) deleted_ids: Vec<ID>,
    /// The ones that stayed, each with its reason.
    pub(crate) skipped: Vec<Skipped>,
}

/// Delete run records.
///
/// Deleting a run takes its sub-agents with it: their records only mean
/// anything under the run that started them. A live run is skipped rather than
/// removed, and deleting a record is not editing a run, so a finished one is
/// fair game.
pub(crate) async fn delete_runs(
    ctx: &Context<'_>,
    request: DeleteRunsRequest,
) -> async_graphql::Result<DeleteRunsResult> {
    let state = ctx.data_unchecked::<AppState>();
    let ids = matching_run_ids(state, request.filter, "delete")
        .await
        .gql()?;
    let outcome = run_core::delete(state, run_core::DeleteTargets::Ids(ids), request.force)
        .await
        .gql()?;
    Ok(DeleteRunsResult {
        deleted_ids: outcome.deleted.into_iter().map(ID::from).collect(),
        skipped: outcome
            .skipped
            .into_iter()
            .map(|skipped| Skipped {
                id: ID::from(skipped.id),
                reason: skip_reason(&skipped.reason),
                message: skipped.reason,
            })
            .collect(),
    })
}
