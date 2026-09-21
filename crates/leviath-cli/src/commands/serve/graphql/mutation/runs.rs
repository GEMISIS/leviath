//! The `pauseRun`, `resumeRun`, `spawnRun`, `sendMessage`, `deleteRuns` and
//! `cancelRun` fields: the acts a run itself goes through, and the wait for
//! one of them to show in the run's own record before answering.

use async_graphql::{Context, InputObject, SimpleObject};

use super::super::super::core::error::ServeError;
use super::super::super::core::lifecycle::{self, Action};
use super::super::super::core::runs as run_core;
use super::super::super::core::spawn as spawn_core;
use super::super::super::types::AppState;
use super::super::error::IntoGraphql;
use super::super::inputs::{BlueprintInput, RegionInput};
use super::super::scalars::Timestamp;
use super::super::types::run::Run;
use crate::runstate;

/// Seed text for one context region at spawn.
#[derive(InputObject)]
pub(crate) struct RegionSeedInput {
    /// The region to seed.
    pub(crate) region: RegionInput,
    /// The text it starts with.
    pub(crate) text: String,
}

/// One caller-supplied metadata entry.
#[derive(InputObject)]
pub(crate) struct MetadataEntryInput {
    /// The key.
    pub(crate) key: String,
    /// The value. Always a string.
    pub(crate) value: String,
}

/// Everything about a new run.
#[derive(InputObject)]
pub(crate) struct SpawnRunInput {
    /// The blueprint to start. A `digest` on it refuses the spawn where what is
    /// installed under that name is a different revision, which is how a client
    /// starts the blueprint it read rather than whatever is there now.
    pub(crate) blueprint: BlueprintInput,
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
    /// Run unattended: approvals resolve without a person. Refused outright on
    /// a server started with `--no-remote-yolo`.
    ///
    /// Null is `false`, not a third state: the run asks a person. Only
    /// `yoloProfile` says which prompts are waived rather than all of them.
    pub(crate) yolo: Option<bool>,
    /// A named yolo profile, which is a kind of yolo and refused with it.
    pub(crate) yolo_profile: Option<String>,
    /// Tools to allow without asking, for this run.
    pub(crate) allow: Option<Vec<String>>,
    /// Refuse this blueprint's command seeds, which run before any approval
    /// prompt exists.
    ///
    /// Null is `false`, not a third state: the seeds the blueprint declares run.
    pub(crate) no_seed_commands: Option<bool>,
    /// Write this run's exact requests into its journal, once per provider
    /// attempt, whatever this machine is configured to do for other runs.
    ///
    /// A captured request is the whole prompt, holding whatever the run's
    /// context held: file contents, command output, the words somebody typed.
    /// There is no size cap, and every call re-sends the window, so a captured
    /// run's journal grows by roughly the context size per attempt. Read it back
    /// on `InferenceAttempt.modelInput`.
    pub(crate) capture_model_input: Option<bool>,
    /// Seed text for named context regions.
    pub(crate) regions: Option<Vec<RegionSeedInput>>,
    /// Caller-supplied metadata: labels for whoever started the run, such as a
    /// ticket or a tenant. Values are always strings, the run reads none of
    /// them, and `filter.query` searches them. Not a typed extension point. A
    /// key given twice keeps the last value.
    pub(crate) metadata: Option<Vec<MetadataEntryInput>>,
    /// The output format label to ask the run for.
    pub(crate) output_format: Option<String>,
    /// Extra instructions for the run's output stage.
    pub(crate) output_instructions: Option<String>,
    /// URL the daemon POSTs this run's events to. Checked against the same
    /// outbound policy a model-supplied URL is.
    pub(crate) callback_url: Option<String>,
    /// Shared secret for signing that webhook. Write-only: never read back on
    /// the run.
    ///
    /// Refused without a `callbackUrl`, since there is no webhook to sign:
    /// sending one on its own means a caller believes it has set up a signed
    /// callback that will never fire. Send both, or neither.
    pub(crate) callback_secret: Option<String>,
}

/// One run a delete passed over, and why.
#[derive(Debug, SimpleObject)]
pub(crate) struct SkippedDelete {
    /// The run that stayed.
    pub(crate) id: String,
    /// Why it did: it is still going, or its record cannot be read.
    pub(crate) reason: String,
}

/// What a delete removed, and what it left.
///
/// Partial success is the normal outcome rather than an edge case: a sweep
/// names runs by a predicate, and one of them being live is no reason to refuse
/// the rest. Read `skipped` when the list does not empty.
#[derive(Debug, SimpleObject)]
pub(crate) struct DeletePayload {
    /// The runs that were removed, sub-agents included.
    pub(crate) deleted: Vec<String>,
    /// The ones that stayed, each with its reason.
    pub(crate) skipped: Vec<SkippedDelete>,
}

/// What a lifecycle mutation answers with.
#[derive(SimpleObject)]
pub(crate) struct RunPayload {
    /// The run as the act left it.
    ///
    /// The daemon applies the act to its world before it answers, and writes
    /// the record a moment later, so this waits for the act to show there
    /// before answering: a pause reads `PAUSED`, a cancel reads `CANCELLED`,
    /// and a resume reads whatever the run went back to doing.
    ///
    /// A run slow to write its record is answered with the record as it stands
    /// rather than held any longer. That is not a refusal - a refusal is an
    /// error, never a quiet answer - so a status that does not yet show the act
    /// means the act is still on its way. Watch `RunStatusChanged` to see it
    /// land.
    pub(crate) run: Run,
    /// Retired checks the mutation noticed. Empty unless something was
    /// superseded.
    pub(crate) warnings: Vec<String>,
}

/// The output shape a spawn request asks for, when it asks for one.
fn output_spec(input: &SpawnRunInput) -> Option<leviath_core::output::OutputSpec> {
    if input.output_format.is_none() && input.output_instructions.is_none() {
        return None;
    }
    Some(leviath_core::output::OutputSpec {
        format: input.output_format.clone(),
        instructions: input.output_instructions.clone(),
        ..leviath_core::output::OutputSpec::default()
    })
}

/// Read a run back after a mutation moved it.
fn read_back(run_id: &str, warnings: Vec<String>) -> Result<RunPayload, ServeError> {
    let meta = runstate::read_meta(run_id).map_err(|e| {
        // The daemon accepted the act, so the run exists. A record that will
        // not read is this server's problem, and saying "not found" about a run
        // that just moved would blame the caller.
        ServeError::Internal(format!(
            "Run '{run_id}' changed, but its record would not read: {e}"
        ))
    })?;
    Ok(RunPayload {
        run: Run {
            meta: std::sync::Arc::new(meta),
            now: leviath_core::duration::now_secs(),
        },
        warnings,
    })
}

/// Carry out one lifecycle action and read the run back.
/// How long to keep looking for the act in the run's record before answering
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

async fn act_and_read(
    ctx: &Context<'_>,
    run_id: &str,
    action: Action,
) -> async_graphql::Result<RunPayload> {
    let state = ctx.data_unchecked::<AppState>();
    lifecycle::act(state, run_id, action).await.gql()?;
    let meta = settle(action, std::time::Instant::now() + SETTLE_WINDOW, || {
        runstate::read_meta(run_id).map_err(|e| {
            // The daemon accepted the act, so the run exists. A record that
            // will not read is this server's problem, not the caller's, and
            // saying so beats answering "not found" about a run that just
            // moved.
            ServeError::Internal(format!(
                "Run '{run_id}' changed, but its record would not read: {e}"
            ))
        })
    })
    .await
    .gql()?;
    Ok(RunPayload {
        run: Run {
            meta: std::sync::Arc::new(meta),
            now: leviath_core::duration::now_secs(),
        },
        warnings: Vec::new(),
    })
}

/// Park a run.
///
/// Read `run.status` on the way back: `PAUSED` means the pause landed.
/// A finished run is a `CONFLICT`, never a silent no-op.
pub(crate) async fn pause_run(
    ctx: &Context<'_>,
    run_id: String,
) -> async_graphql::Result<RunPayload> {
    act_and_read(ctx, &run_id, Action::Pause).await
}

/// Resume a paused run.
///
/// Read `run.status`: `RUNNING` means it is moving again.
pub(crate) async fn resume_run(
    ctx: &Context<'_>,
    run_id: String,
) -> async_graphql::Result<RunPayload> {
    act_and_read(ctx, &run_id, Action::Resume).await
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
pub(crate) async fn spawn_run(
    ctx: &Context<'_>,
    input: SpawnRunInput,
) -> async_graphql::Result<RunPayload> {
    let state = ctx.data_unchecked::<AppState>();
    let max_depth = match input.max_depth {
        None => None,
        Some(depth) => Some(
            usize::try_from(depth)
                .map_err(|_| ServeError::BadRequest("`maxDepth` cannot be negative".to_string()))
                .gql()?,
        ),
    };
    let output = output_spec(&input);
    let blueprint = input.blueprint.installed(state).await.gql()?;
    let request = spawn_core::SpawnRequest {
        blueprint,
        task: input.task,
        model: input.model,
        max_depth,
        workdir: input.workdir,
        yolo: input.yolo.unwrap_or(false),
        yolo_profile: input.yolo_profile,
        allow: input.allow.unwrap_or_default(),
        no_seed_commands: input.no_seed_commands.unwrap_or(false),
        capture_model_input: input.capture_model_input.unwrap_or(false),
        regions: input
            .regions
            .into_iter()
            .flatten()
            .map(|seed| (seed.region.name, seed.text))
            .collect(),
        metadata: input
            .metadata
            .into_iter()
            .flatten()
            .map(|entry| (entry.key, entry.value))
            .collect(),
        callback_url: input.callback_url,
        callback_secret: input.callback_secret,
        output,
    };
    // No file parts on this path yet: the ones a run starts with are named
    // by the REST multipart route, and adding them here is a schema
    // addition rather than a change.
    let spawned = spawn_core::spawn(state, request, Vec::new()).await.gql()?;
    read_back(&spawned.run_id, spawned.warnings).gql()
}

/// Send a message to a run that is going.
///
/// Whether it lands is the daemon's call: a stage that declared
/// `accepts_messages = false`, or a finished run, does not take one, and the
/// refusal says that rather than claiming the run does not exist.
pub(crate) async fn send_message(
    ctx: &Context<'_>,
    run_id: String,
    message: String,
    target_region: Option<RegionInput>,
) -> async_graphql::Result<RunPayload> {
    let state = ctx.data_unchecked::<AppState>();
    let target_region = target_region.map(|region| region.name);
    spawn_core::send_message(state, &run_id, message, target_region, Vec::new())
        .await
        .gql()?;
    read_back(&run_id, Vec::new()).gql()
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
pub(crate) async fn delete_runs(
    ctx: &Context<'_>,
    ids: Option<Vec<String>>,
    before: Option<Timestamp>,
    force: bool,
) -> async_graphql::Result<DeletePayload> {
    let state = ctx.data_unchecked::<AppState>();
    let targets = match (ids, before) {
        (Some(ids), None) => run_core::DeleteTargets::Ids(ids),
        (None, Some(before)) => run_core::DeleteTargets::Before(before.0),
        (Some(_), Some(_)) => {
            return Err(ServeError::BadRequest(
                "`ids` and `before` are two predicates for one delete; send one".to_string(),
            ))
            .gql();
        }
        (None, None) => {
            return Err(ServeError::BadRequest(
                "a delete needs `ids` or `before`; refusing to delete every run".to_string(),
            ))
            .gql();
        }
    };
    let outcome = run_core::delete(state, targets, force).await.gql()?;
    Ok(DeletePayload {
        deleted: outcome.deleted,
        skipped: outcome
            .skipped
            .into_iter()
            .map(|skipped| SkippedDelete {
                id: skipped.id,
                reason: skipped.reason,
            })
            .collect(),
    })
}

/// Cancel a run, and its sub-agents with it.
///
/// Read `run.status`: `CANCELLED` means the cancel landed. A run that had
/// already finished is a `CONFLICT`, which tells a client the difference
/// between "you stopped it" and "it was over before you asked".
pub(crate) async fn cancel_run(
    ctx: &Context<'_>,
    run_id: String,
) -> async_graphql::Result<RunPayload> {
    act_and_read(ctx, &run_id, Action::Cancel).await
}
