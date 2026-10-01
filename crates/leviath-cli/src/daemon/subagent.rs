//! Sub-agent tool handlers: turn the sub-agent tool calls into
//! [`SubAgentOp`]s serviced by the host (which owns the world and the run
//! starter). The tool lane runs off the world, so it blocks on the host
//! applying each op via a oneshot, the same shape as an interaction.
//!
//! - `spawn_agent` and `validate_spawn` read their arguments into a
//!   [`SpawnRequest`](leviath_runtime::spec::request::SpawnRequest) (see
//!   [`args`]) and hand it to the host, which resolves it as a child of the
//!   calling run. A refusal comes back as a numbered list of every problem.
//! - `check_agent`, `wait_for_agent`, `send_to_agent` and `kill_agent` act on a
//!   child by its run id.
//! - `spawn_schema`, `describe_blueprint` and `run_history` read without
//!   starting anything (see [`reads`]).

mod args;
mod reads;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use leviath_core::region::EntryContent;
use leviath_providers::ToolCall;
use leviath_runtime::components::AgentStatus;
use leviath_runtime::host::{SubAgentOp, SubAgentReport};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use args::{SpawnCall, refusal};

/// Per-agent state needed to service the sub-agent tools: a sender into the
/// host's [`SubAgentOp`] channel plus the spawning agent's identity and the
/// context children inherit.
#[derive(Clone)]
pub(crate) struct SubAgentHandle {
    /// Sender into the host's sub-agent op channel.
    pub sender: UnboundedSender<SubAgentOp>,
    /// The run id of the agent that owns this handle (the would-be parent).
    pub parent_run_id: String,
    /// Working directory children inherit.
    pub workdir: String,
    /// The parent run's `--no-seed-commands` setting, inherited by children so a
    /// per-run opt-out can't be side-stepped by spawning a sub-agent whose
    /// blueprint declares command seeds.
    pub no_seed_commands: bool,
    /// The parent run's `--yolo` setting, inherited by children.
    ///
    /// A child spawned attended under an unattended parent stops at its first
    /// approval prompt with nobody there to answer, and takes the parent down
    /// with it whenever the parent is waiting on it. The operator asked for an
    /// unattended run; the tree is the run.
    pub unattended: bool,
    /// The parent run's yolo profile, inherited with `unattended`: a child of
    /// a `careful` run is a `careful` run, not a bare `--yolo` one.
    pub yolo_profile: Option<String>,
    /// The tools the parent run may call without asking: what a child asks
    /// for when its call names none, narrowed by the host as always.
    pub allow: Vec<String>,
    /// The parent run's `--model` override, inherited by children.
    ///
    /// The docs call the override absolute - it "overrides everything" - and a
    /// child named by the model at run time is part of the run, not a separate
    /// one. Without this a spawned sub-agent quietly resolves against its own
    /// blueprint's model list instead. `None` when the run named no model,
    /// which leaves every child resolving from its blueprint.
    pub model_override: Option<String>,
    /// The parent's stored parts, as the runtime last offered them to the
    /// tool lane: what `spawn_agent`'s `parts` names.
    pub offered_parts: Arc<std::sync::Mutex<Vec<leviath_core::mime::Part>>>,
    /// The parent's blob store, to read a named part's bytes from. `None`
    /// in a world with no store, where `parts` is refused.
    pub mime: Option<Arc<leviath_tools::ToolMime>>,
    /// Where installed blueprints live, for `describe_blueprint`. `None`
    /// when there is no home directory.
    pub agents_dir: Option<PathBuf>,
}

/// The parts `spawn_agent`'s `parts` argument names, read from the parent's
/// store as inbound parts for the child, which stores them again under its
/// own run. A name that matches nothing, or bytes the store no longer holds,
/// refuses the spawn: a child started without the file its parent meant to
/// hand it would work from a stand-in and never know.
fn parts_for_child(
    h: &SubAgentHandle,
    wanted: &[String],
    limit: Option<&[String]>,
) -> Result<Vec<leviath_core::mime::InboundPart>, String> {
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let Some(mime) = h.mime.as_deref() else {
        return Err(
            "this run has no blob store, so it has no parts to hand a sub-agent".to_string(),
        );
    };
    let offered = h
        .offered_parts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    wanted
        .iter()
        .map(|name| {
            let (part, blob) = offered
                .iter()
                .filter_map(|p| p.blob().map(|b| (p, b)))
                .find(|(p, _)| leviath_scripting::parts::part_matches(p, name))
                .ok_or_else(|| {
                    format!(
                        "'{name}' names no stored part of this run (a part's name or sha256 prefix)"
                    )
                })?;
            // The stage's limit for this tool, when it has one.
            if let Some(limit) = limit
                && !blob.mime_type.matches_any(limit)
            {
                return Err(format!(
                    "'{name}' is {}; at this stage spawn_agent may be handed only {}",
                    blob.mime_type,
                    limit.join(", ")
                ));
            }
            let bytes = mime
                .store
                .read(&mime.run_id, &blob.sha256)
                .map_err(|e| format!("'{name}' could not be read from the store: {e}"))?;
            let mut inbound = leviath_core::mime::InboundPart::from_bytes(
                part.name
                    .clone()
                    .unwrap_or_else(|| blob.short_sha().to_string()),
                bytes.to_vec(),
            )
            .typed(blob.mime_type.clone());
            inbound.deliver = part.deliver;
            Ok(inbound)
        })
        .collect()
}

// The sub-agent tool-name list lives in `leviath-tools` (next to the tool
// defs), shared with the runtime's crash-replay synthesis; re-exported here for
// the existing dispatch-routing callers.
#[cfg(test)]
use leviath_tools::SUBAGENT_TOOLS;
pub(crate) use leviath_tools::is_subagent_tool;

/// How often `wait_for_agent` / `spawn_agent(wait=true)` polls the child.
const WAIT_POLL: Duration = Duration::from_millis(500);

/// Dispatch one sub-agent tool call, returning the textual result for the model.
#[cfg(test)]
pub(crate) async fn handle(h: &SubAgentHandle, tc: &ToolCall) -> String {
    handle_within(h, tc, None).await
}

/// [`handle`], with what the stage lets `spawn_agent` be handed
/// (`tool_accepts`), when it limits it: the text view of
/// [`handle_content`].
#[cfg(test)]
pub(crate) async fn handle_within(
    h: &SubAgentHandle,
    tc: &ToolCall,
    limit: Option<&[String]>,
) -> String {
    handle_content(h, tc, limit).await.into_string()
}

/// Dispatch one sub-agent tool call, keeping the files a finished child
/// handed back as parts on the result rather than flattening them into its
/// text.
///
/// The tool lane's result is an entry, so a child that drew or built
/// something reaches its parent's model as the file itself when the model
/// takes the type, and as the stand-in line otherwise. `limit` is what the
/// stage lets `spawn_agent` be handed (`tool_accepts`), when it limits it.
pub(crate) async fn handle_content(
    h: &SubAgentHandle,
    tc: &ToolCall,
    limit: Option<&[String]>,
) -> EntryContent {
    let args = &tc.arguments;
    match tc.name.as_str() {
        "spawn_agent" => spawn(h, args, limit).await,
        "validate_spawn" => EntryContent::text(validate(h, args, limit).await),
        "check_agent" => check(h, str_arg(args, "agent_id")).await,
        "wait_for_agent" => wait(h, str_arg(args, "agent_id")).await,
        "send_to_agent" => EntryContent::text(send(h, args).await),
        "kill_agent" => EntryContent::text(kill(h, str_arg(args, "agent_id")).await),
        "spawn_schema" => EntryContent::text(reads::spawn_schema(args)),
        "describe_blueprint" => EntryContent::text(reads::describe_blueprint(h, args)),
        "run_history" => EntryContent::text(reads::run_history(h, args).await),
        other => EntryContent::text(format!("[error] '{other}' is not a sub-agent tool")),
    }
}

/// A required string argument, or `""` when missing/not a string.
fn str_arg<'a>(args: &'a serde_json::Value, key: &str) -> &'a str {
    args.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

/// Read a spawn call into the request it asks for, with the named parts
/// attached, and whether the caller wants to wait for the child.
fn child_request(
    h: &SubAgentHandle,
    args: &serde_json::Value,
    limit: Option<&[String]>,
) -> Result<
    (leviath_runtime::spec::request::SpawnRequest, bool),
    leviath_runtime::spec::issues::SpawnIssues,
> {
    use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpecPath};
    let args = SpawnCall::parse(args)?;
    let attachments = parts_for_child(h, &args.parts, limit)
        .map_err(|e| SpawnIssue::new(SpecPath::root().field("parts"), IssueCode::Invalid, e))?;
    let wait = args.wait;
    Ok((args.into_request(h, attachments)?, wait))
}

async fn spawn(
    h: &SubAgentHandle,
    args: &serde_json::Value,
    limit: Option<&[String]>,
) -> EntryContent {
    let (request, wait_flag) = match child_request(h, args, limit) {
        Ok(ok) => ok,
        Err(issues) => return EntryContent::text(refusal("spawn_agent", &issues)),
    };
    let (tx, rx) = oneshot::channel();
    if h.sender
        .send(SubAgentOp::Spawn {
            request: Box::new(request),
            parent_run_id: h.parent_run_id.clone(),
            reply: tx,
        })
        .is_err()
    {
        return EntryContent::text("[error] the daemon is shutting down");
    }
    match rx.await {
        Ok(Ok(child_id)) if wait_flag => wait(h, child_id.as_str()).await,
        Ok(Ok(child_id)) => EntryContent::text(format!("Spawned sub-agent '{child_id}'.")),
        Ok(Err(issues)) => EntryContent::text(refusal("spawn_agent", &issues)),
        Err(_) => EntryContent::text("[error] the daemon dropped the spawn request"),
    }
}

/// `validate_spawn`: everything `spawn_agent` would check, with nothing
/// started.
async fn validate(
    h: &SubAgentHandle,
    args: &serde_json::Value,
    limit: Option<&[String]>,
) -> String {
    let request = match child_request(h, args, limit) {
        Ok((request, _)) => request,
        Err(issues) => return refusal("validate_spawn", &issues),
    };
    let (tx, rx) = oneshot::channel();
    if h.sender
        .send(SubAgentOp::Validate {
            request: Box::new(request),
            parent_run_id: h.parent_run_id.clone(),
            reply: tx,
        })
        .is_err()
    {
        return "[error] the daemon is shutting down".to_string();
    }
    match rx.await {
        Ok(Ok(summary)) => reads::valid(&summary),
        Ok(Err(issues)) => refusal("validate_spawn", &issues),
        Err(_) => "[error] the daemon dropped the validate request".to_string(),
    }
}

async fn check(h: &SubAgentHandle, agent_id: &str) -> EntryContent {
    match report_of(h, agent_id).await {
        // The tool's schema promises "its current status and result if
        // complete", so a finished child's answer comes back with the status
        // rather than the parent being told only that it finished.
        Some(report) if is_terminal(&report.status) => finished(
            h,
            agent_id,
            format!("Sub-agent '{agent_id}' status: {}", label(&report.status)),
            &report,
        ),
        Some(report) => EntryContent::text(format!(
            "Sub-agent '{agent_id}' status: {}",
            label(&report.status)
        )),
        None => EntryContent::text(format!("[error] no such sub-agent '{agent_id}'")),
    }
}

async fn wait(h: &SubAgentHandle, agent_id: &str) -> EntryContent {
    if agent_id.is_empty() {
        return EntryContent::text("[error] wait_for_agent requires 'agent_id'");
    }
    // The whole wait happens off the tool lane. The child's own tool batches
    // queue on that lane, so a parent that kept lane capacity while waiting
    // would hold the very thing the child needs to finish: parent and child
    // deadlock on each other and the whole factory stops.
    leviath_runtime::tool_bridge::off_lane(poll_until_finished(h, agent_id)).await
}

/// Poll `agent_id` until it reaches a terminal state, or until the caller does.
async fn poll_until_finished(h: &SubAgentHandle, agent_id: &str) -> EntryContent {
    loop {
        match report_of(h, agent_id).await {
            None => return EntryContent::text(format!("[error] no such sub-agent '{agent_id}'")),
            Some(report) if is_terminal(&report.status) => {
                // This is what the tool has always advertised - "block until a
                // sub-agent completes, then return its final result" - and what
                // it never did. A parent that waited got a status label and had
                // to agree on a file path out of band to receive any work.
                return finished(
                    h,
                    agent_id,
                    format!(
                        "Sub-agent '{agent_id}' finished with status: {}",
                        label(&report.status)
                    ),
                    &report,
                );
            }
            // The caller itself was cancelled (or failed) while waiting. Give up
            // rather than keep polling for a child that is being torn down with
            // it - this loop has no other exit, so it would otherwise run for as
            // long as the daemon lived.
            Some(_) if caller_is_terminal(h).await => {
                return EntryContent::text(format!(
                    "[error] cancelled while waiting for '{agent_id}'"
                ));
            }
            Some(_) => tokio::time::sleep(WAIT_POLL).await,
        }
    }
}

/// A finished child's report: the status line, its answer, and the files it
/// handed back as parts of the result.
fn finished(
    h: &SubAgentHandle,
    agent_id: &str,
    status_line: String,
    report: &SubAgentReport,
) -> EntryContent {
    EntryContent::text(format!("{status_line}{}", describe_result(report)))
        .with_parts(handed_back(h, agent_id, report))
}

/// The child's artifacts, stored again under the parent's run as parts
/// named `<child>/<artifact>`.
///
/// The store is content-addressed and shared across runs, so the child's
/// bytes are read by hash and stored under the parent, typed as the child
/// declared them, so a sub-agent that drew or built something hands its
/// parent the file and not a path it cannot read. A file the store no longer
/// holds, or one over the part
/// ceiling, is left out with a warning; a world with no store hands up
/// nothing, and the text still names every file.
fn handed_back(
    h: &SubAgentHandle,
    agent_id: &str,
    report: &SubAgentReport,
) -> Vec<leviath_core::mime::Part> {
    let (Some(mime), Some(output)) = (h.mime.as_deref(), report.final_output.as_ref()) else {
        return Vec::new();
    };
    output
        .artifacts
        .iter()
        .filter(|a| {
            let stored = !a.sha256.is_empty();
            if !stored {
                tracing::warn!(
                    child = %agent_id,
                    part = %a.name,
                    "[mime] sub-agent artifact not handed up: the child never stored it"
                );
            }
            stored
        })
        .filter_map(|artifact| {
            let name = format!("{agent_id}/{}", artifact.name);
            let stored = mime
                .store
                .read(agent_id, &artifact.sha256)
                .map_err(|e| e.to_string())
                .and_then(|bytes| {
                    let mut blob =
                        leviath_core::mime::Blob::new(artifact.mime_type.clone(), bytes.to_vec());
                    blob.name = Some(name.clone());
                    mime.store(blob)
                });
            match stored {
                Ok(part) => Some(part),
                Err(why) => {
                    tracing::warn!(child = %agent_id, part = %name, "[mime] sub-agent artifact not handed up: {why}");
                    None
                }
            }
        })
        .collect()
}

/// Whether the agent that called `wait_for_agent` has itself reached a terminal
/// state. A dropped request (daemon shutting down) counts as terminal - there is
/// nothing left to wait for either way.
async fn caller_is_terminal(h: &SubAgentHandle) -> bool {
    match status_of(h, &h.parent_run_id).await {
        Some(status) => is_terminal(&status),
        None => true,
    }
}

async fn send(h: &SubAgentHandle, args: &serde_json::Value) -> String {
    let agent_id = str_arg(args, "agent_id");
    let message = str_arg(args, "message");
    if agent_id.is_empty() || message.is_empty() {
        return "[error] send_to_agent requires 'agent_id' and 'message'".to_string();
    }
    // Empty string means unset, same as absent: delivery defaults to the
    // conversation region, which is what the tool's schema documents.
    let target_region = Some(str_arg(args, "target_region"))
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let (tx, rx) = oneshot::channel();
    if h.sender
        .send(SubAgentOp::Send {
            run_id: agent_id.to_string(),
            caller_run_id: h.parent_run_id.clone(),
            content: message.to_string(),
            target_region,
            reply: tx,
        })
        .is_err()
    {
        return "[error] the daemon is shutting down".to_string();
    }
    match rx.await {
        Ok(true) => format!("Delivered message to '{agent_id}'."),
        Ok(false) => format!(
            "[error] '{agent_id}' did not accept the message. An agent may only \
             message itself or an agent it spawned."
        ),
        Err(_) => "[error] the daemon dropped the message".to_string(),
    }
}

async fn kill(h: &SubAgentHandle, agent_id: &str) -> String {
    if agent_id.is_empty() {
        return "[error] kill_agent requires 'agent_id'".to_string();
    }
    let (tx, rx) = oneshot::channel();
    if h.sender
        .send(SubAgentOp::Kill {
            run_id: agent_id.to_string(),
            caller_run_id: h.parent_run_id.clone(),
            reply: tx,
        })
        .is_err()
    {
        return "[error] the daemon is shutting down".to_string();
    }
    match rx.await {
        Ok(true) => format!("Killed sub-agent '{agent_id}' and its descendants."),
        Ok(false) => format!("[error] no such sub-agent '{agent_id}'"),
        Err(_) => "[error] the daemon dropped the kill request".to_string(),
    }
}

/// Query a child's status via the host, `None` if it dropped the request or the
/// run is unknown.
async fn report_of(h: &SubAgentHandle, agent_id: &str) -> Option<SubAgentReport> {
    let (tx, rx) = oneshot::channel();
    h.sender
        .send(SubAgentOp::Check {
            run_id: agent_id.to_string(),
            reply: tx,
        })
        .ok()?;
    rx.await.ok().flatten()
}

/// Just the status, for the callers that only need to know whether a run is
/// still going.
async fn status_of(h: &SubAgentHandle, agent_id: &str) -> Option<AgentStatus> {
    report_of(h, agent_id).await.map(|r| r.status)
}

/// Render a finished child's answer for its parent to read.
///
/// A child that submitted nothing says so rather than reporting an empty
/// result: "produced no final output" is actionable (the parent can ask, or
/// route around it), and a bare status line looks like success.
fn describe_result(report: &SubAgentReport) -> String {
    match &report.final_output {
        Some(output) => {
            let shape = output
                .format
                .as_deref()
                .map(|f| format!(" ({f})"))
                .unwrap_or_default();
            let truncated = match output.truncated {
                true => "\n[the agent's output was truncated at the size limit]",
                false => "",
            };
            // The files, named so the parent can tell them apart from its own
            // and read them by the same name the parts carry.
            let files: String = output
                .artifacts
                .iter()
                .map(|a| format!("\n- {}", a.short_label()))
                .collect();
            let files = match files.is_empty() {
                true => String::new(),
                false => format!("\n\n--- files handed back ---{files}"),
            };
            format!(
                "\n\n--- final output{shape} ---\n{}{truncated}{files}",
                output.content
            )
        }
        None => "\n\n[this agent produced no final output]".to_string(),
    }
}

fn is_terminal(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Complete | AgentStatus::Cancelled | AgentStatus::Error { .. }
    )
}

/// What the parent model is told a child's status is. `Display` rather than
/// `label` so a failed child reports why it failed, which is the whole reason
/// the parent asked.
fn label(status: &AgentStatus) -> String {
    status.to_string()
}

#[cfg(test)]
mod tests;
