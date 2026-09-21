//! `lev msg` / `lev cancel` / `lev pause` / `lev resume` - control operations
//! on a running agent in the shared-world daemon.
//!
//! Each sends a control request over the daemon socket and reports the boolean
//! outcome. The request/response cores are tested here; the socket-path
//! resolution + connect live in the binary behind [`crate::dispatch::RiskyExecutors`].

use anyhow::bail;
use leviath_core::interaction::{
    ApprovalScope, InteractionKind, InteractionRequest, InteractionResponse,
};
use leviath_runtime::control_socket::{ControlClient, ControlRequest, ControlResponse};

/// Arguments for `lev msg`.
#[derive(clap::Args, Debug, Clone)]
pub struct MsgArgs {
    /// The target agent id.
    pub agent_id: String,
    /// The message to deliver. A `@path` inside it attaches that file.
    pub content: String,
    /// Attach a file to the message: `path[:region][:type][:text]`, as on
    /// `lev run --attach`. Repeatable.
    #[arg(long, value_name = "PATH[:REGION][:TYPE][:text]")]
    pub attach: Vec<String>,
}

/// Arguments for `lev cancel`.
#[derive(clap::Args, Debug, Clone)]
pub struct CancelArgs {
    /// The run id to cancel.
    pub run_id: String,
    /// Terminate the run's on-disk state directly, without asking the daemon.
    ///
    /// Use when the daemon is gone or unresponsive. The run is recorded
    /// `Cancelled` so nothing lists it as live; if a daemon is in fact still
    /// driving it, restart the daemon so it picks up the new state.
    #[arg(long)]
    pub force: bool,
}

/// Arguments for `lev pause`.
#[derive(clap::Args, Debug, Clone)]
pub struct PauseArgs {
    /// The run id to pause.
    pub run_id: String,
}

/// Arguments for `lev resume`.
#[derive(clap::Args, Debug, Clone)]
pub struct ResumeArgs {
    /// The run id to resume.
    pub run_id: String,
}

/// Arguments for `lev respond` - answer a pending `ask_user` interaction the
/// daemon is holding, or (with no `request_id`) list the open interactions.
#[derive(clap::Args, Debug, Clone)]
pub struct RespondArgs {
    /// The interaction request id to answer, or enough of its start to name
    /// one open interaction. Omit to list open interactions.
    pub request_id: Option<String>,
    /// Free-text (or edited) answer value.
    pub value: Option<String>,
    /// Answer a multiple-choice interaction by 0-based option index.
    #[arg(long)]
    pub choice: Option<usize>,
    /// Approve a tool-approval / confirm interaction.
    #[arg(long, conflicts_with = "deny")]
    pub approve: bool,
    /// Deny a tool-approval / confirm interaction.
    #[arg(long)]
    pub deny: bool,
    /// With `--deny`, tell the model what to do instead. Reaches it as part of
    /// the tool result, so its next turn is a redirect rather than a guess.
    #[arg(long, requires = "deny", value_name = "TEXT")]
    pub feedback: Option<String>,
    /// With `--approve`, allow what this call runs for the rest of the run.
    #[arg(long, visible_alias = "run")]
    pub session: bool,
    /// With `--approve`, allow what this call runs until the run leaves the
    /// current stage.
    #[arg(long, conflicts_with = "session")]
    pub stage: bool,
    /// Report open interactions (or the outcome of answering one) as JSON.
    /// This is how an unattended caller finds the questions it has to answer.
    #[arg(long)]
    pub json: bool,
    /// Attach a file to a text answer: `path[:region][:type][:text]`, as on
    /// `lev run --attach`. Repeatable. A `@path` inside the answer attaches
    /// that file too.
    #[arg(long, value_name = "PATH[:REGION][:TYPE][:text]")]
    pub attach: Vec<String>,
}

/// One open interaction in `lev respond --json`.
///
/// The whole request rather than the four fields the prose listing has room
/// for: `tool_arguments` and `body` are exactly what a caller deciding whether
/// to approve needs, and neither appears in the human listing.
#[derive(serde::Serialize)]
struct OpenInteraction<'a> {
    /// The agent holding the question, for a caller polling several runs.
    agent_id: &'a str,
    #[serde(flatten)]
    request: &'a InteractionRequest,
}

/// Send `request` and report the boolean outcome: `ok` prints `applied_msg`, a
/// `false` outcome the `not_found_msg`. A non-`Ok` response or a connect failure
/// is an error.
async fn send_bool(
    client: &ControlClient,
    request: ControlRequest,
    applied_msg: &str,
    not_found_msg: &str,
) -> anyhow::Result<()> {
    match client.request(&request).await {
        Ok(ControlResponse::Ok { ok: true }) => {
            println!("{applied_msg}");
            Ok(())
        }
        Ok(ControlResponse::Ok { ok: false }) => bail!("{not_found_msg}"),
        Ok(other) => bail!("unexpected daemon response: {other:?}"),
        Err(e) => bail!("the leviath daemon is not reachable ({e}); start it with `lev daemon`"),
    }
}

/// `lev msg`: deliver a message to a running agent.
pub async fn send_message(client: &ControlClient, args: &MsgArgs) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_default();
    let (content, parts) = message_parts(&args.content, &args.attach, &cwd)?;
    send_bool(
        client,
        ControlRequest::Message {
            agent_id: args.agent_id.clone(),
            content,
            target_region: None,
            parts,
        },
        "message delivered",
        "no agent accepted the message",
    )
    .await
}

/// The message text and the parts it carries: every `--attach` file, then
/// every `@path` the text names. A token that names no file stays text and
/// is reported on stderr.
pub(crate) fn message_parts(
    text: &str,
    attach: &[String],
    cwd: &std::path::Path,
) -> anyhow::Result<(String, Vec<leviath_core::mime::InboundPart>)> {
    let mut parts = crate::commands::run::attach::attach_all(attach, cwd)?;
    let (text, named, unresolved) = crate::commands::run::attach::inline_parts(text, None, cwd)?;
    parts.extend(named);
    crate::commands::run::attach::warn_unresolved(&unresolved);
    Ok((text, parts))
}

/// `lev pause`: park a run. The daemon refuses (`ok: false`) when the run does
/// not exist or is not in a pausable state (waiting on input, or finished).
pub async fn pause_run(client: &ControlClient, args: &PauseArgs) -> anyhow::Result<()> {
    send_bool(
        client,
        ControlRequest::Pause {
            run_id: args.run_id.clone(),
        },
        "paused",
        "no such run, or it is not pausable in its current state",
    )
    .await
}

/// `lev resume`: un-pause a run.
pub async fn resume_run(client: &ControlClient, args: &ResumeArgs) -> anyhow::Result<()> {
    send_bool(
        client,
        ControlRequest::Resume {
            run_id: args.run_id.clone(),
        },
        "resumed",
        "no such run, or it is not paused",
    )
    .await
}

/// `lev cancel`: cancel a run.
///
/// A kill must always be possible, so this never depends on the daemon being
/// reachable. `--force` goes straight to the run's on-disk state; otherwise the
/// daemon is asked first (it can also stop the work, not just record the
/// outcome) and the on-disk write is the fallback when it can't be reached or
/// doesn't answer in time.
pub async fn cancel_run(client: &ControlClient, args: &CancelArgs) -> anyhow::Result<()> {
    if args.force {
        return report_forced(
            crate::runstate::force_cancel(&args.run_id),
            &args.run_id,
            None,
        );
    }
    match client
        .request(&ControlRequest::Cancel {
            run_id: args.run_id.clone(),
        })
        .await
    {
        Ok(ControlResponse::Ok { ok: true }) => {
            println!("cancelled");
            Ok(())
        }
        Ok(ControlResponse::Ok { ok: false }) => bail!("no such run"),
        Ok(other) => bail!("unexpected daemon response: {other:?}"),
        // The daemon is down, wedged, or too busy to answer. Terminate the run on
        // disk ourselves rather than leave the user with nothing.
        Err(e) => report_forced(
            crate::runstate::force_cancel(&args.run_id),
            &args.run_id,
            Some(e),
        ),
    }
}

/// Report the outcome of an on-disk cancel. `daemon_error` is set when this was
/// a fallback rather than an explicit `--force`, and is included so the user
/// knows why the daemon wasn't used.
fn report_forced(
    outcome: crate::runstate::ForceCancelOutcome,
    run_id: &str,
    daemon_error: Option<std::io::Error>,
) -> anyhow::Result<()> {
    use crate::runstate::ForceCancelOutcome as O;
    let why = match &daemon_error {
        Some(e) => format!(" (the daemon did not answer: {e})"),
        None => String::new(),
    };
    match outcome {
        O::Terminated => {
            println!(
                "cancelled '{run_id}' on disk{why}; if a daemon is still running, \
                 restart it so it picks up the change"
            );
            Ok(())
        }
        O::AlreadyTerminal => {
            println!("'{run_id}' had already finished; nothing to cancel");
            Ok(())
        }
        O::NoSuchRun => match daemon_error {
            Some(e) => bail!(
                "the leviath daemon is not reachable ({e}), and there is no run '{run_id}' on disk"
            ),
            None => bail!("no such run"),
        },
        O::WriteFailed => bail!("could not write '{run_id}' metadata to record the cancel"),
    }
}

/// A short human label for an interaction kind (used by the `lev respond` list).
fn kind_label(kind: &InteractionKind) -> &'static str {
    match kind {
        InteractionKind::FreeText => "free-text",
        InteractionKind::MultipleChoice => "choice",
        InteractionKind::Confirm => "confirm",
        InteractionKind::ToolApproval => "tool-approval",
        InteractionKind::EditText => "edit-text",
    }
}

/// The line that identifies one open interaction: its id and where it came
/// from. Heads its listing entry, and is what a refusal lists its candidates
/// as, so the two read the same.
fn interaction_headline(agent_id: &str, req: &InteractionRequest) -> String {
    format!(
        "{}  [{}]  agent={}  stage={}",
        req.id,
        kind_label(&req.kind),
        agent_id,
        req.stage_name
    )
}

/// Render one open interaction as a multi-line listing entry.
fn format_interaction(agent_id: &str, req: &InteractionRequest) -> String {
    let mut s = format!("{}\n  {}", interaction_headline(agent_id, req), req.prompt);
    for (i, opt) in req.options.iter().enumerate() {
        s.push_str(&format!("\n    {i}) {opt}"));
    }
    if let Some(tool) = &req.tool_name {
        s.push_str(&format!("\n    tool: {tool}"));
    }
    s
}

/// Build the [`InteractionResponse`] implied by the CLI flags. Approve/deny wins,
/// then an explicit `--choice`, otherwise a free-text value (empty if omitted).
fn build_response(request_id: &str, args: &RespondArgs) -> InteractionResponse {
    if args.approve || args.deny {
        let scope = match (args.session, args.stage) {
            (true, _) => ApprovalScope::Run,
            (_, true) => ApprovalScope::Stage,
            _ => ApprovalScope::Once,
        };
        match args.feedback.as_deref() {
            // `requires = "deny"` keeps this off the approve path, so a
            // feedback here is always a deny.
            Some(feedback) => InteractionResponse::deny_with_feedback(request_id, feedback),
            None => InteractionResponse::approval(request_id, args.approve, scope),
        }
    } else if let Some(index) = args.choice {
        InteractionResponse::choice(request_id, index)
    } else {
        InteractionResponse::text(request_id, args.value.clone().unwrap_or_default())
    }
}

/// Put the files a text answer names on the answer: every `--attach`, then
/// every `@path` in the value. A choice or an approval has no text for a
/// file to sit beside, so `--attach` on one is refused rather than dropped.
fn attach_answer(
    mut response: InteractionResponse,
    attach: &[String],
    cwd: &std::path::Path,
) -> anyhow::Result<InteractionResponse> {
    let Some(value) = response.value.as_deref() else {
        if !attach.is_empty() {
            bail!(
                "--attach goes with a text answer; a choice or an approval has no text for a \
                 file to sit beside"
            );
        }
        return Ok(response);
    };
    let (text, parts) = message_parts(value, attach, cwd)?;
    response.value = Some(text);
    response.parts = parts;
    Ok(response)
}

/// `--feedback` is a deny's message and nothing else. clap's `requires`
/// catches it on its own; beside `--approve` the parser lets it through, and
/// a redirect silently dropped on a grant is the one outcome nobody asked for.
fn check_feedback_flag(args: &RespondArgs) -> anyhow::Result<()> {
    if args.feedback.is_some() && !args.deny {
        bail!("--feedback goes with --deny: it tells the model what to do instead of the call");
    }
    Ok(())
}

/// Every interaction the daemon is currently holding.
async fn open_interactions(
    client: &ControlClient,
) -> anyhow::Result<Vec<(String, InteractionRequest)>> {
    match client.request(&ControlRequest::ListInteractions).await {
        Ok(ControlResponse::Interactions { interactions }) => Ok(interactions),
        Ok(other) => bail!("unexpected daemon response: {other:?}"),
        Err(e) => bail!("the leviath daemon is not reachable ({e}); start it with `lev daemon`"),
    }
}

/// List the interactions the daemon is currently holding.
async fn list_interactions(client: &ControlClient, json: bool) -> anyhow::Result<()> {
    let interactions = open_interactions(client).await?;
    if json {
        let open: Vec<OpenInteraction<'_>> = interactions
            .iter()
            .map(|(agent_id, request)| OpenInteraction { agent_id, request })
            .collect();
        // Nothing open is an empty array, not a sentence: a caller
        // polling this branches on length, not on prose.
        println!(
            "{}",
            serde_json::to_string_pretty(&open).expect("an interaction listing serializes")
        );
        return Ok(());
    }
    if interactions.is_empty() {
        println!("no open interactions");
    } else {
        for (agent_id, req) in &interactions {
            println!("{}", format_interaction(agent_id, req));
        }
    }
    Ok(())
}

/// Which open interaction `typed` names.
///
/// A request id given in full is that interaction, whatever longer ids it
/// happens to start: a full id names one request by construction, so there is
/// nothing to weigh. Anything shorter is read as the start of an id, and it
/// has to leave exactly one candidate. Several is refused with all of them
/// listed - a request id carries the run that raised it, so a prompt answered
/// against the wrong one lets work nobody looked at through, and four more
/// characters is the cheaper of the two.
fn resolve_request_id(
    typed: &str,
    open: &[(String, InteractionRequest)],
) -> anyhow::Result<String> {
    if typed.is_empty() {
        bail!("name the interaction to answer; `lev respond` with no id lists the open ones");
    }
    if open.iter().any(|(_, req)| req.id == typed) {
        return Ok(typed.to_string());
    }
    let named: Vec<&(String, InteractionRequest)> = open
        .iter()
        .filter(|(_, req)| req.id.starts_with(typed))
        .collect();
    match named.as_slice() {
        [] => bail!("no such open interaction"),
        [(_, req)] => Ok(req.id.clone()),
        several => {
            let candidates = several
                .iter()
                .map(|(agent_id, req)| format!("  {}", interaction_headline(agent_id, req)))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "'{typed}' is the start of {} open interactions, so nothing was answered; \
                 give enough of an id to name just one:\n{candidates}",
                several.len()
            )
        }
    }
}

/// The success line for an answer: a request id typed in full says nothing a
/// bare `answered` doesn't, but one grown from a prefix names what it reached.
fn answered_line(typed: &str, request_id: &str) -> String {
    match typed == request_id {
        true => "answered".to_string(),
        false => format!("answered {request_id}"),
    }
}

/// `lev respond <id>`: answer the interaction `typed` names.
async fn answer_interaction(
    client: &ControlClient,
    args: &RespondArgs,
    typed: &str,
) -> anyhow::Result<()> {
    // The files the answer carries are read first, so a path that does not
    // exist is the file's error rather than whatever the daemon says next.
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut response = attach_answer(build_response(typed, args), &args.attach, &cwd)?;
    let request_id = resolve_request_id(typed, &open_interactions(client).await?)?;
    response.request_id = request_id.clone();
    // A failed answer stays an error (non-zero exit plus the message on
    // stderr), so `--json` only changes the success line.
    let applied = match args.json {
        // Serialized, not interpolated: a request id carrying a quote
        // would otherwise emit JSON that does not parse.
        true => serde_json::json!({ "answered": true, "request_id": request_id }).to_string(),
        false => answered_line(typed, &request_id),
    };
    send_bool(
        client,
        ControlRequest::AnswerInteraction { response },
        &applied,
        "no such open interaction",
    )
    .await
}

/// `lev respond`: answer a pending interaction, or list open ones when no
/// `request_id` is given.
pub async fn respond(client: &ControlClient, args: &RespondArgs) -> anyhow::Result<()> {
    check_feedback_flag(args)?;
    match &args.request_id {
        None => list_interactions(client, args.json).await,
        Some(typed) => answer_interaction(client, args, typed).await,
    }
}

#[cfg(test)]
#[path = "ctl_tests.rs"]
mod tests;
