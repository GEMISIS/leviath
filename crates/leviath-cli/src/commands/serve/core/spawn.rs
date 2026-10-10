//! Starting a run, and steering one that is already going.
//!
//! A run is asked for with a [`Request`]: the one typed shape every front
//! door builds. The daemon checks it, resolves it and starts it, and a request
//! it refuses comes back with every problem at once. What this server adds is
//! what only it can know, since it sits between a caller on the network and
//! the machine:
//!
//! - a blueprint read from a directory is for a caller on this machine;
//! - a workdir has to sit under `--workdir-root`;
//! - an unattended run, or one with tools allowed outright, is refused on a
//!   server started with `--no-remote-yolo`;
//! - a webhook URL has to pass the same outbound policy a model-supplied URL
//!   does;
//! - seed commands are refused on a server started with
//!   `--no-remote-seed-commands`.
//!
//! The first four are [`SpawnIssue`]s like the daemon's own, so a caller with
//! a refused workdir and a misspelled input hears about both in one answer:
//! this server asks the daemon for its issues too before it answers. The last
//! is not a refusal: such a run starts with its seed commands off.

use leviath_core::mime::InboundPart;
use leviath_runtime::control_socket::{ControlRequest, ControlResponse};
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use leviath_runtime::spec::launch::Unattended;
use leviath_runtime::spec::names::BlueprintPath;
use leviath_runtime::spec::request::{SpawnRequest as Request, SpawnSource};
use leviath_runtime::spec::summary::SpawnSummary;

use super::super::types::AppState;
use super::error::ServeError;
use super::held;

/// What the daemon, or this server, made of a request.
#[derive(Debug)]
pub(crate) enum Verdict<T> {
    /// It went ahead: the new run's id, or what the run would be.
    Accepted(T),
    /// It was refused, with every reason.
    Rejected(SpawnIssues),
}

/// A run that started: its id, and what may keep it from ever finishing.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Started {
    /// The new run's id.
    pub(crate) run_id: String,
    /// What may keep it from ever finishing. Empty for most runs.
    pub(crate) warnings: SpawnIssues,
}

/// Start the run `request` asks for.
///
/// `Ok(Rejected)` is a request that cannot run as written; `Err` is a server
/// or a daemon that could not answer.
pub(crate) async fn start(
    state: &AppState,
    mut request: Request,
) -> Result<Verdict<Started>, ServeError> {
    if let Some(issues) = refused(state, &mut request).await? {
        return Ok(Verdict::Rejected(issues));
    }
    match state.control.spawn(request).await {
        Ok(ControlResponse::Spawned { run_id, warnings }) => {
            // No spawned frame from here. The daemon emits one for every run
            // the world gains, however it was launched, so a second one would
            // make exactly the runs that arrived over the network appear twice.
            tracing::info!(run_id = %run_id, "spawned a run via the API");
            Ok(Verdict::Accepted(Started { run_id, warnings }))
        }
        Ok(ControlResponse::Rejected { issues }) => Ok(Verdict::Rejected(issues)),
        Ok(other) => Err(daemon_refusal(&other)),
        Err(e) => Err(ServeError::from_daemon_io(&e)),
    }
}

/// What `request` would run, without running anything.
pub(crate) async fn validate(
    state: &AppState,
    mut request: Request,
) -> Result<Verdict<SpawnSummary>, ServeError> {
    if let Some(issues) = refused(state, &mut request).await? {
        return Ok(Verdict::Rejected(issues));
    }
    match state.control.validate_spawn(request).await {
        Ok(ControlResponse::Valid { summary }) => Ok(Verdict::Accepted(*summary)),
        Ok(ControlResponse::Rejected { issues }) => Ok(Verdict::Rejected(issues)),
        Ok(other) => Err(daemon_refusal(&other)),
        Err(e) => Err(ServeError::from_daemon_io(&e)),
    }
}

/// A reply to a spawn or a dry run that is neither an answer nor issues: the
/// daemon shutting down, or a reply to some other question.
fn daemon_refusal(reply: &ControlResponse) -> ServeError {
    match reply {
        ControlResponse::Error { message } => ServeError::DaemonUnavailable(message.clone()),
        other => ServeError::unexpected_reply(other),
    }
}

/// Apply this server's rules to `request`. `Some` when any refuses it, with
/// the daemon's own issues added; `None` when the request may go ahead, as
/// adjusted.
async fn refused(
    state: &AppState,
    request: &mut Request,
) -> Result<Option<SpawnIssues>, ServeError> {
    let local = request.check_remote().err();
    if local.is_none() {
        installed_elsewhere(state, request).await;
    }
    let mut issues = server_issues(state, request).await;
    if issues.is_empty() && local.is_none() {
        return Ok(None);
    }
    // The daemon reads the blueprint a request names, so one this server
    // refused to read is not handed on to be read for its other issues.
    if let Some(refusal) = local {
        issues.absorb(refusal);
        return Ok(Some(issues));
    }
    match state.control.validate_spawn(request.clone()).await {
        Ok(ControlResponse::Rejected { issues: more }) => issues.absorb(more),
        Ok(ControlResponse::Valid { .. }) => {}
        Ok(other) => return Err(daemon_refusal(&other)),
        Err(e) => return Err(ServeError::from_daemon_io(&e)),
    }
    Ok(Some(issues))
}

/// The refusals only this server can make, for `request` as it arrived, with
/// its workdir filled in and its seed commands turned off where this server
/// turns them off.
async fn server_issues(state: &AppState, request: &mut Request) -> SpawnIssues {
    let limits = &state.limits;
    let mut issues = SpawnIssues::new();
    let named = request.workdir.is_some();
    let workdir = request
        .workdir
        .get_or_insert_with(|| std::env::current_dir().unwrap_or_default());
    // `--workdir-root` is the operator's answer to "where is this API allowed
    // to work": without it, a caller-supplied `"/"` would point a
    // tool-executing run at the whole filesystem. A request that names no
    // workdir works where the server started, which is held to the same
    // root, and the refusal says that is where the directory came from.
    if let Err(message) = limits.check_workdir(workdir) {
        let message = match named {
            true => message,
            false => format!(
                "the request names no workdir, so the run would work in the directory the \
                 server was started in: {message}"
            ),
        };
        issues.push(
            SpawnIssue::new(
                SpecPath::root().field("workdir"),
                IssueCode::NotAllowed,
                message,
            )
            .hint("send a workdir under the server's --workdir-root"),
        );
    }
    // `unattended` and `allow` are the same lever: an allow list approves
    // tools without a person just as an unattended run does, so a server that
    // refuses one refuses both. Any `allow` is refused rather than only a
    // wildcard: a per-agent grant belongs in the operator's own config.
    let launch = &mut request.launch;
    if limits.no_remote_yolo && launch.unattended != Unattended::Off {
        issues.push(
            SpawnIssue::new(
                SpecPath::root().field("launch").field("unattended"),
                IssueCode::NotAllowed,
                "this server refuses unattended runs (--no-remote-yolo)",
            )
            .expected("off"),
        );
    }
    if limits.no_remote_yolo && !launch.allow.is_empty() {
        issues.push(
            SpawnIssue::new(
                SpecPath::root().field("launch").field("allow"),
                IssueCode::NotAllowed,
                "this server refuses tools allowed outright (--no-remote-yolo)",
            )
            .hint("leave `allow` empty; the operator grants tools in the server's own config"),
        );
    }
    if limits.no_remote_seed_commands {
        launch.seed_commands = false;
    }
    // On the blocking pool: the check resolves the callback's host, and a
    // resolver can take seconds to answer.
    let callback = request
        .delivery
        .callback
        .as_ref()
        .map(|c| c.url.to_string());
    let limits = std::sync::Arc::clone(limits);
    let checked = super::super::blocking::blocking(move || {
        callback.map(|url| limits.check_callback_url(&url))
    })
    .await;
    if let Some(Err(message)) = checked {
        issues.push(SpawnIssue::new(
            SpecPath::root()
                .field("delivery")
                .field("callback")
                .field("url"),
            IssueCode::NotAllowed,
            message,
        ));
    }
    issues
}

/// Point a request for a blueprint this server lists from a configured
/// `agent_paths` directory, rather than the installed ones, at that
/// directory. The daemon resolves names against the installed blueprints only,
/// and the operator listing a directory is what puts its blueprints on offer
/// here. A request pinned to a revision is left alone: a directory has no
/// revision to pin.
async fn installed_elsewhere(state: &AppState, request: &mut Request) {
    let SpawnSource::Blueprint(reference) = &request.source else {
        return;
    };
    let name = reference.name.to_string();
    let installed = super::super::blueprints::agents_dir()
        .join(&name)
        .join(leviath_blueprint::FILE_NAME)
        .is_file();
    if installed || reference.digest.is_some() {
        return;
    }
    let roots = state.current_config().agent_paths.clone();
    let listed =
        super::super::blocking::blocking(move || super::super::blueprints::discover_in(roots))
            .await;
    let path = listed
        .into_iter()
        .find(|blueprint| blueprint.name == name)
        .and_then(|found| std::fs::canonicalize(found.path).ok())
        .and_then(|dir| BlueprintPath::new(dir.to_string_lossy()).ok());
    if let Some(path) = path {
        request.source = SpawnSource::BlueprintFile(path);
    }
}

/// Deliver a message to a run that is going.
///
/// The daemon decides whether the run takes one: a stage that declared
/// `accepts_messages = false`, or a finished run, does not, and the refusal
/// says so rather than claiming the run does not exist.
pub(crate) async fn send_message(
    state: &AppState,
    run_id: &str,
    message: String,
    target_region: Option<String>,
    parts: Vec<InboundPart>,
) -> Result<(), ServeError> {
    let says_nothing = message.trim().is_empty() && parts.is_empty();
    let reply = state
        .control
        .request(&ControlRequest::Message {
            agent_id: run_id.to_string(),
            content: message,
            target_region,
            parts,
        })
        .await;
    match reply {
        Ok(ControlResponse::Ok { ok: true }) => Ok(()),
        Ok(ControlResponse::Ok { ok: false }) => Err(ServeError::NotFound(format!(
            "Agent run '{run_id}' is not accepting messages"
        ))),
        // A message that says nothing.
        Ok(ControlResponse::Error { message }) if says_nothing => {
            Err(ServeError::BadRequest(message))
        }
        // Refused because of the run it names: a run held off this machine
        // says what to put back, one with a record here (finished, failed or
        // cancelled) is there and refuses it, and no record is a miss. The
        // daemon's words, which say what to do, are kept as they are.
        Ok(ControlResponse::Error { message }) => {
            Err(match held::is_held(&state.control, run_id).await {
                true => ServeError::Held(message),
                false if crate::runstate::read_meta(run_id).is_ok() => {
                    ServeError::Conflict(message)
                }
                false => ServeError::NotFound(message),
            })
        }
        Ok(other) => Err(ServeError::unexpected_reply(&other)),
        Err(e) => Err(ServeError::from_daemon_io(&e)),
    }
}

/// What an answer to a question nothing has open is told: it was answered
/// already, or it expired.
const NOT_OPEN: &str = "No such open interaction: it was answered already, or it expired";

/// Answer a pending ask.
///
/// The first answer wins: the daemon takes the request out of its pending map
/// under a lock, so a second answer to the same request finds nothing and is
/// told so. Two people clicking the same prompt is the ordinary case, not an
/// error worth hiding. A question a held run asked is refused as held, with
/// what to put back, rather than told it is not there.
pub(crate) async fn answer_interaction(
    state: &AppState,
    response: leviath_core::interaction::InteractionResponse,
) -> Result<(), ServeError> {
    let request_id = response.request_id.clone();
    let reply = state
        .control
        .request(&ControlRequest::AnswerInteraction { response })
        .await;
    match reply {
        Ok(ControlResponse::Ok { ok: true }) => Ok(()),
        Ok(ControlResponse::Ok { ok: false }) => Err(held::or_held(
            &state.control,
            |h| h.question.id == request_id,
            ServeError::NotFound(NOT_OPEN.to_string()),
        )
        .await),
        // An answer the question cannot take, such as text for a choice or
        // nothing at all. The question stays open.
        Ok(ControlResponse::Error { message }) => Err(ServeError::BadRequest(message)),
        Ok(other) => Err(ServeError::unexpected_reply(&other)),
        Err(e) => Err(ServeError::from_daemon_io(&e)),
    }
}

/// Every open ask across every run: the approval inbox.
///
/// The daemon holds these in memory, so this is one indexed read rather than a
/// walk of the run store.
pub(crate) async fn open_interactions(
    state: &AppState,
) -> Result<Vec<(String, leviath_core::interaction::InteractionRequest)>, ServeError> {
    match state
        .control
        .request(&ControlRequest::ListInteractions)
        .await
    {
        Ok(ControlResponse::Interactions { interactions }) => Ok(interactions),
        Ok(other) => Err(ServeError::unexpected_reply(&other)),
        Err(e) => Err(ServeError::from_daemon_io(&e)),
    }
}

/// The ask open under `request_id`, read from the daemon.
///
/// Nothing open under it is `NotFound`: answered already, or expired. A
/// question a held run asked is `Held` instead, saying what to put back.
pub(crate) async fn open_request(
    state: &AppState,
    request_id: &str,
) -> Result<leviath_core::interaction::InteractionRequest, ServeError> {
    let open = open_interactions(state).await?;
    match open.into_iter().find(|(_, ask)| ask.id == request_id) {
        Some((_, ask)) => Ok(ask),
        None => Err(held::or_held(
            &state.control,
            |h| h.question.id == request_id,
            ServeError::NotFound(NOT_OPEN.to_string()),
        )
        .await),
    }
}

/// The ask run `run_id` is parked on, read from the daemon.
///
/// None is `NotFound`. A run the daemon holds off this machine with a question
/// open is `Held`: nothing can answer that question until the run is back.
pub(crate) async fn run_interaction(
    state: &AppState,
    run_id: &str,
) -> Result<leviath_core::interaction::InteractionRequest, ServeError> {
    let open = open_interactions(state).await?;
    match open.into_iter().find(|(id, _)| id == run_id) {
        Some((_, ask)) => Ok(ask),
        None => Err(held::or_held(
            &state.control,
            |h| h.run_id == run_id,
            ServeError::NotFound("No pending interaction".to_string()),
        )
        .await),
    }
}

#[cfg(test)]
#[path = "spawn_tests.rs"]
mod tests;
