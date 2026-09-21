//! Starting a run, and steering one that is already going.
//!
//! What a surface contributes is decoding: REST takes JSON or multipart and
//! resolves `@path` tokens against the workdir, GraphQL takes a typed input.
//! What happens after that is here, so the refusals are the same either way: a
//! workdir outside `--workdir-root`, a yolo flag on a server that refuses them,
//! a callback URL the outbound policy will not allow.

use leviath_core::mime::InboundPart;
use leviath_runtime::control_socket::{ControlRequest, ControlResponse};
use leviath_runtime::host::SpawnArgs;

use super::super::types::AppState;
use super::error::ServeError;
use crate::runstate;

/// Everything about a new run, after the request that carried it is decoded.
///
/// The files are separate ([`spawn`] takes them alongside) because each surface
/// resolves them its own way: bytes uploaded in a multipart body, or paths
/// named inside the run's workdir.
pub(crate) struct SpawnRequest {
    /// The blueprint to start, by name.
    pub(crate) blueprint: String,
    /// The initial ask.
    pub(crate) task: String,
    /// Override the blueprint's model for this run.
    pub(crate) model: Option<String>,
    /// How deep sub-agent spawning may nest for this run.
    pub(crate) max_depth: Option<usize>,
    /// Where the run's tools execute. Defaults to this server's own directory.
    pub(crate) workdir: Option<String>,
    /// Run unattended: approvals resolve without a person.
    pub(crate) yolo: bool,
    /// The named yolo profile, which is a kind of yolo and refused with it.
    pub(crate) yolo_profile: Option<String>,
    /// Tools to allow outright for this run.
    pub(crate) allow: Vec<String>,
    /// Refuse this run's command seeds.
    pub(crate) no_seed_commands: bool,
    /// Seed text per region, by region name.
    pub(crate) regions: std::collections::HashMap<String, String>,
    /// Caller-supplied metadata. Values are always strings.
    pub(crate) metadata: std::collections::HashMap<String, String>,
    /// URL the daemon POSTs run events to.
    pub(crate) callback_url: Option<String>,
    /// Shared secret for signing that webhook. Never read back.
    pub(crate) callback_secret: Option<String>,
    /// The shape this caller wants the answer in.
    pub(crate) output: Option<leviath_core::output::OutputSpec>,
    /// Write this run's exact requests into its journal, whatever the machine's
    /// own `[observability] capture_model_input` says.
    pub(crate) capture_model_input: bool,
}

/// A run that was started.
#[derive(Debug)]
pub(crate) struct Spawned {
    /// The new run's id.
    pub(crate) run_id: String,
    /// Retired checks the spawn noticed. Empty when there were none.
    pub(crate) warnings: Vec<String>,
}

/// Start a run.
///
/// The refusals, in the order they are checked, because each one is about a
/// different decision: the blueprint has to exist, the workdir has to be
/// somewhere this server is allowed to work, an unattended run has to be
/// allowed on this server at all, and a webhook URL has to pass the same
/// outbound policy a model-supplied URL does.
pub(crate) async fn spawn(
    state: &AppState,
    request: SpawnRequest,
    parts: Vec<InboundPart>,
) -> Result<Spawned, ServeError> {
    // A secret with nowhere to go is refused rather than accepted and dropped.
    // It signs the webhook body, so a caller that sends one and no URL believes
    // it has set up a signed callback, and the run will never call anything: the
    // mistake is silent for the whole life of the run, and the thing it silently
    // loses is a credential.
    if request.callback_secret.is_some() && request.callback_url.is_none() {
        return Err(ServeError::BadRequest(
            "`callback_secret` signs the callback body, so it needs a \
             `callback_url` to sign for. Send both, or neither."
                .to_string(),
        ));
    }
    let config = state.current_config();
    let roots = super::super::blueprints::blueprint_roots(&config);
    let installed =
        super::super::blocking::blocking(move || super::super::blueprints::discover_in(roots))
            .await;
    let found = installed
        .iter()
        .find(|blueprint| blueprint.name == request.blueprint)
        .ok_or_else(|| {
            ServeError::NotFound(format!("Blueprint '{}' not found", request.blueprint))
        })?;
    let manifest_path =
        std::path::PathBuf::from(&found.path).join(leviath_core::files::MANIFEST_FILENAME);

    let workdir = request.workdir.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .map(|dir| dir.to_string_lossy().to_string())
            .unwrap_or_default()
    });
    // `--workdir-root` is the operator's answer to "where is this API allowed to
    // work": without it, a caller-supplied `"/"` would point a tool-executing run
    // at the whole filesystem.
    state
        .limits
        .check_workdir(std::path::Path::new(&workdir))
        .map_err(ServeError::Forbidden)?;
    // A named profile is a kind of yolo, so it is refused with it.
    let yolo = request.yolo || request.yolo_profile.is_some();
    state
        .limits
        .check_launch_overrides(yolo, &request.allow)
        .map_err(ServeError::Forbidden)?;
    if let Some(callback) = request.callback_url.as_deref() {
        state
            .limits
            .check_callback_url(callback)
            .map_err(ServeError::Forbidden)?;
    }

    let args = SpawnArgs {
        run_id: runstate::new_run_id(&request.blueprint),
        blueprint_path: manifest_path.to_string_lossy().to_string(),
        task: request.task,
        regions: request.regions,
        model: request.model,
        workdir,
        metadata: request.metadata,
        callback_url: request.callback_url,
        callback_secret: request.callback_secret,
        yolo,
        yolo_profile: request.yolo_profile,
        // Either side may refuse: this caller for this run, or the operator for
        // every run that arrives over the network.
        no_seed_commands: request.no_seed_commands || state.limits.no_remote_seed_commands,
        allow: request.allow,
        output: request.output,
        max_depth: request.max_depth,
        // A run started through this server is a top-level run.
        parent_run_id: None,
        worker_stage: None,
        parts,
        capture_model_input: request.capture_model_input,
    };
    // What this spawn tells its caller alongside the run id: the declared
    // checks that the request's own output shape retires. The daemon logs the
    // same retirement into its own log, which the caller never reads, and the
    // check they may be counting on deserves a line in the response they do.
    // Best-effort: a manifest that will not read is the daemon's to report as
    // the spawn error, and this must never be why one fails.
    let warnings = crate::commands::run::manifest::retired_check_warnings_at(
        &manifest_path,
        args.output.as_ref(),
    );
    let blueprint = request.blueprint;

    match state.control.spawn(args).await {
        Ok(ControlResponse::Spawned { run_id }) => {
            // No spawned frame from here. The daemon emits one for every run the
            // world gains, however it was launched, so a second one would make
            // exactly the runs that arrived over the network appear twice.
            tracing::info!(run_id = %run_id, blueprint = %blueprint, "spawned agent via API");
            Ok(Spawned { run_id, warnings })
        }
        // The daemon's own refusal: a manifest that will not load, a region the
        // blueprint does not declare, a seed that cannot run.
        Ok(ControlResponse::Error { message }) => Err(ServeError::BadRequest(format!(
            "Failed to spawn agent: {message}"
        ))),
        Ok(other) => Err(ServeError::unexpected_reply(&other)),
        Err(e) => Err(ServeError::from_daemon_io(&e)),
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
        Ok(other) => Err(ServeError::unexpected_reply(&other)),
        Err(e) => Err(ServeError::from_daemon_io(&e)),
    }
}

/// Answer a pending ask.
///
/// The first answer wins: the daemon takes the request out of its pending map
/// under a lock, so a second answer to the same request finds nothing and is
/// told so. Two people clicking the same prompt is the ordinary case, not an
/// error worth hiding.
pub(crate) async fn answer_interaction(
    state: &AppState,
    response: leviath_core::interaction::InteractionResponse,
) -> Result<(), ServeError> {
    let reply = state
        .control
        .request(&ControlRequest::AnswerInteraction { response })
        .await;
    match reply {
        Ok(ControlResponse::Ok { ok: true }) => Ok(()),
        Ok(ControlResponse::Ok { ok: false }) => Err(ServeError::NotFound(
            "No such open interaction: it was answered already, or it expired".to_string(),
        )),
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

#[cfg(test)]
#[path = "spawn_tests.rs"]
mod tests;
