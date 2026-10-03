//! `lev msg` / `lev cancel` / `lev pause` / `lev resume` - control operations
//! on a running agent in the shared-world daemon.
//!
//! Each sends a control request over the daemon socket and reports the boolean
//! outcome. The request/response cores are tested here; the socket-path
//! resolution + connect live in the binary behind [`crate::dispatch::RiskyExecutors`].

use anyhow::bail;
use leviath_core::interaction::{
    AnswerOption, ApprovalScope, InteractionKind, InteractionRequest, InteractionResponse,
    answer_options, how_to_answer,
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

/// The rule `lev respond --help` closes on: every kind of question is answered
/// with what it shows.
const RESPOND_HELP: &str = "\
Answer with what the question shows. `lev interactions` lists each option
numbered from 1, beside the word that answers with it:
  tool approval    allow, allow-stage, allow-run or deny (deny takes --feedback)
  confirm          yes or no
  multiple choice  the option, or any start of it that names just one
  free text, edit  the text itself (\"\" is an answer too)
The number an option is listed under answers with it as well.

To see an interaction before answering it: lev interactions <REQUEST_ID>";

/// Arguments for `lev respond` - answer an interaction the daemon is holding.
///
/// Answering is all it does, and only with an answer given: an answer can't be
/// taken back, so naming a question is never enough to answer it.
/// `lev interactions` lists and shows them.
#[derive(clap::Args, Debug, Clone)]
#[command(after_help = RESPOND_HELP)]
pub struct RespondArgs {
    /// The interaction request id to answer, or enough of its start to name
    /// one open interaction. `lev interactions` lists them.
    pub request_id: String,
    /// The answer, read the way the question asks: an approval takes allow,
    /// allow-stage, allow-run or deny, a confirm yes or no, a multiple choice
    /// the option or the start of it, and each of those the number its
    /// option is listed under. A free-text or edit question takes the text
    /// as written; an empty "" acknowledges a review, or keeps an edited
    /// document as it was.
    #[arg(value_name = "ANSWER")]
    pub value: Option<String>,
    /// Answer with the option at this 0-based position in the listing, for
    /// any question that lists options. The scripting form of a numbered
    /// answer: `--choice 0` is the option listed as 1.
    #[arg(long)]
    pub choice: Option<usize>,
    /// Approve a tool approval, or say yes to a confirm: the same as `allow`
    /// or `yes`.
    #[arg(long, conflicts_with = "deny")]
    pub approve: bool,
    /// Deny a tool approval, or say no to a confirm: the same as `deny` or
    /// `no`.
    #[arg(long)]
    pub deny: bool,
    /// With a deny, tell the model what to do instead. Reaches it as part of
    /// the tool result, so its next turn is a redirect rather than a guess.
    #[arg(long, value_name = "TEXT")]
    pub feedback: Option<String>,
    /// With `--approve`, allow what this call runs for the rest of the run:
    /// the same as `allow-run`.
    #[arg(long, visible_alias = "run")]
    pub session: bool,
    /// With `--approve`, allow what this call runs until the run leaves the
    /// current stage: the same as `allow-stage`.
    #[arg(long, conflicts_with = "session")]
    pub stage: bool,
    /// Report the outcome of the answer as JSON.
    #[arg(long)]
    pub json: bool,
    /// Attach a file to a text answer: `path[:region][:type][:text]`, as on
    /// `lev run --attach`. Repeatable. A `@path` inside the answer attaches
    /// that file too.
    #[arg(long, value_name = "PATH[:REGION][:TYPE][:text]")]
    pub attach: Vec<String>,
}

/// Arguments for `lev interactions` - list the interactions the daemon is
/// holding, or show one in full. Reading never answers anything.
#[derive(clap::Args, Debug, Clone)]
pub struct InteractionsArgs {
    /// Show this interaction in full: its request id, or enough of its start
    /// to name one. Omit to list every open interaction.
    pub request_id: Option<String>,
    /// Report as JSON. This is how an unattended caller finds the questions
    /// it has to answer.
    #[arg(long)]
    pub json: bool,
}

/// One open interaction in `lev interactions --json`.
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
    /// Each option as the listing numbers it, with the word and the command
    /// that answer with it. Empty for a text question.
    answer_options: Vec<AnswerOption>,
}

impl<'a> OpenInteraction<'a> {
    fn new(agent_id: &'a str, request: &'a InteractionRequest) -> Self {
        Self {
            agent_id,
            request,
            answer_options: answer_options(request),
        }
    }
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
        // The daemon refused the request and said why.
        Ok(ControlResponse::Error { message }) => bail!("{message}"),
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
    let parts = crate::commands::run::attach::attach_all(attach, cwd)?;
    with_named_parts(text, parts, cwd)
}

/// `text` as the model reads it, and `parts` followed by every file a `@path`
/// in it names. A token that names no file stays text and is reported on
/// stderr.
fn with_named_parts(
    text: &str,
    mut parts: Vec<leviath_core::mime::InboundPart>,
    cwd: &std::path::Path,
) -> anyhow::Result<(String, Vec<leviath_core::mime::InboundPart>)> {
    let (text, named, unresolved) = crate::commands::run::attach::inline_parts(text, None, cwd)?;
    parts.extend(named);
    crate::commands::run::attach::warn_unresolved(&unresolved);
    Ok((text, parts))
}

/// `lev pause`: park a run. The daemon refuses (`ok: false`) when the run does
/// not exist or is not in a pausable state (waiting on its sub-agents, or
/// finished); a run that is paused already is said to be.
pub async fn pause_run(client: &ControlClient, args: &PauseArgs) -> anyhow::Result<()> {
    let refused = send_bool(
        client,
        ControlRequest::Pause {
            run_id: args.run_id.clone(),
        },
        "paused",
        "no such run, or it is not pausable in its current state",
    )
    .await;
    let Err(refusal) = refused else {
        return Ok(());
    };
    match client.status(&args.run_id).await {
        Ok(ControlResponse::Status {
            status: Some(leviath_runtime::components::AgentStatus::Paused),
        }) => bail!(
            "run '{}' is already paused; `lev resume {}` carries it on",
            args.run_id,
            args.run_id
        ),
        _ => Err(refusal),
    }
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

/// A short human label for an interaction kind (used by the `lev interactions` list).
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

/// One option as the listing shows it: its number, its label, and what to
/// type to answer with it.
fn format_option(req: &InteractionRequest, option: &AnswerOption) -> String {
    let prefix = format!("lev respond {} ", req.id);
    let typed = option
        .answer
        .strip_prefix(&prefix)
        .unwrap_or(&option.answer);
    format!("\n    [{}] {}  ({typed})", option.number, option.label)
}

/// The part of a listing entry the full view shares: where it came from, the
/// question, and its options numbered from 1.
fn format_interaction_head(agent_id: &str, req: &InteractionRequest) -> String {
    let mut s = format!("{}\n  {}", interaction_headline(agent_id, req), req.prompt);
    for option in answer_options(req) {
        s.push_str(&format_option(req, &option));
    }
    if let Some(tool) = &req.tool_name {
        s.push_str(&format!("\n    tool: {tool}"));
    }
    s
}

/// Render one open interaction as a multi-line listing entry, ending in the
/// line that answers it.
fn format_interaction(agent_id: &str, req: &InteractionRequest) -> String {
    format!(
        "{}\n  answer with: {}",
        format_interaction_head(agent_id, req),
        how_to_answer(req)
    )
}

/// Render one open interaction in full, for `lev interactions <id>`: all of
/// what the listing entry says, then what the listing has no room for (the
/// call's arguments, the document under review), and the line that answers it.
fn format_interaction_detail(agent_id: &str, req: &InteractionRequest) -> String {
    let mut s = format_interaction_head(agent_id, req);
    if let Some(arguments) = &req.tool_arguments {
        s.push_str("\n  arguments:");
        let pretty = serde_json::to_string_pretty(arguments).expect("JSON serializes");
        for line in pretty.lines() {
            s.push_str(&format!("\n    {line}"));
        }
    }
    if let Some(body) = &req.body {
        s.push_str("\n  body:");
        for line in body.lines() {
            s.push_str(&format!("\n    {line}"));
        }
    }
    let required = match req.required {
        true => "yes",
        false => "no",
    };
    s.push_str(&format!("\n  required: {required}"));
    s.push_str(&format!("\nanswer with: {}", how_to_answer(req)));
    s
}

/// `lev respond` has to be told what the answer is: exactly one of an answer,
/// `--choice`, `--approve` or `--deny`. An answer can't be taken back, and one
/// with nothing in it reads to the run like nobody answered (a checkpoint
/// approves, a review is "acknowledged"), so none given is refused rather than
/// sent. Two given is refused too, rather than one quietly dropped.
fn check_one_answer(args: &RespondArgs) -> anyhow::Result<()> {
    let given = [
        args.value.is_some(),
        args.choice.is_some(),
        args.approve,
        args.deny,
    ];
    match given.into_iter().filter(|given| *given).count() {
        1 => Ok(()),
        0 => bail!(
            "refusing to answer '{id}' without an answer: give an ANSWER (allow, deny, yes, \
             no, an option, its number, or the text). To see the question first: lev \
             interactions {id}",
            id = args.request_id
        ),
        _ => bail!(
            "give one answer: an ANSWER, --choice, --approve and --deny are each a whole answer"
        ),
    }
}

/// Build the [`InteractionResponse`] that answers `request` the way the
/// arguments say. `--approve` and `--deny` answer as they always have,
/// `--choice` picks an option by its 0-based place in the listing, and the
/// answer is read the way the question asks for it. [`check_one_answer`] has
/// already made sure exactly one was given.
fn build_response(
    request: &InteractionRequest,
    args: &RespondArgs,
) -> Result<InteractionResponse, String> {
    let request_id = request.id.as_str();
    let feedback = args.feedback.as_deref();
    if args.approve || args.deny {
        let scope = match (args.session, args.stage) {
            (true, _) => ApprovalScope::Run,
            (_, true) => ApprovalScope::Stage,
            _ => ApprovalScope::Once,
        };
        // [`check_flags`] keeps feedback off the approve path, so a feedback
        // here is always a deny.
        return Ok(match feedback {
            Some(feedback) => InteractionResponse::deny_with_feedback(request_id, feedback),
            None => InteractionResponse::approval(request_id, args.approve, scope),
        });
    }
    match args.choice {
        Some(index) => leviath_core::interaction::answer_at(request, index, feedback),
        None => leviath_core::interaction::parse_answer(
            request,
            args.value.as_deref().unwrap_or_default(),
            feedback,
        ),
    }
}

/// Put the files a text answer names on the answer: the `--attach` files,
/// already read, then every `@path` in the value. A choice or an approval has
/// no text for a file to sit beside, so `--attach` on one is refused rather
/// than dropped.
fn attach_answer(
    mut response: InteractionResponse,
    attached: Vec<leviath_core::mime::InboundPart>,
    cwd: &std::path::Path,
) -> anyhow::Result<InteractionResponse> {
    let Some(value) = response.value.as_deref() else {
        if !attached.is_empty() {
            bail!(
                "--attach goes with a text answer; a choice or an approval has no text for a \
                 file to sit beside"
            );
        }
        return Ok(response);
    };
    let (text, parts) = with_named_parts(value, attached, cwd)?;
    response.value = Some(text);
    response.parts = parts;
    Ok(response)
}

/// The flags that only make sense beside one kind of answer.
///
/// `--feedback` is a deny's message and nothing else: beside `--approve` a
/// redirect would be silently dropped on a grant, the one outcome nobody asked
/// for. A deny written as `deny`, `no` or a number is checked against the
/// question, since only the question knows which option that is. `--stage`
/// and `--session` widen `--approve`; an answer written as a word or a number
/// already names its scope.
fn check_flags(args: &RespondArgs) -> anyhow::Result<()> {
    if args.feedback.is_some() && args.approve {
        bail!("--feedback goes with a deny: it tells the model what to do instead of the call");
    }
    if (args.stage || args.session) && (args.value.is_some() || args.choice.is_some()) {
        bail!(
            "--stage and --session go with --approve; answer allow-stage or allow-run to \
             widen a grant"
        );
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

/// A question a run asked that the daemon holds off this machine. Nothing
/// answers it while the run is held: once the machine can take the run back,
/// the question reopens under a new id.
pub(super) struct HeldQuestion {
    run_id: String,
    question: leviath_runtime::state::OpenInteraction,
    remedy: String,
}

/// The questions open on the runs the daemon holds off this machine, read off
/// each one's file. None when the daemon does not list its runs.
async fn held_questions(client: &ControlClient) -> Vec<HeldQuestion> {
    let runs = match client.request(&ControlRequest::List).await {
        Ok(ControlResponse::List { runs, .. }) => runs,
        _ => Vec::new(),
    };
    runs.into_iter()
        .filter_map(|row| match row.wait_reason {
            Some(leviath_core::run_meta::WaitReason::NeedsSetup { remedy, .. }) => {
                Some((row.run_id, remedy))
            }
            _ => None,
        })
        .flat_map(|(run_id, remedy)| {
            let open = crate::runstate::run_file::tail_in(&crate::runstate::run_dir(&run_id))
                .map(|tail| tail.state.interactions)
                .unwrap_or_default();
            open.into_iter().map(move |question| HeldQuestion {
                run_id: run_id.clone(),
                question,
                remedy: remedy.clone(),
            })
        })
        .collect()
}

/// One held question as the listing shows it: what was asked, and why
/// nothing can answer it yet.
fn format_held(held: &HeldQuestion) -> String {
    let q = &held.question;
    let mut s = format!("{}  [held]  agent={}\n  {}", q.id, held.run_id, q.prompt);
    for (i, option) in q.options.iter().enumerate() {
        s.push_str(&format!("\n    {}. {option}", i + 1));
    }
    s.push_str(&format!(
        "\n  held: this machine cannot take the run back as it stands, so nothing can answer \
         this yet: {}\n  once the run is back, the question reopens under a new id",
        held.remedy
    ));
    s
}

/// Why `typed` names no open interaction, when it names a question a held run
/// asked: whole, or the start of its id.
fn held_refusal(typed: &str, held: &[HeldQuestion]) -> Option<String> {
    let h = held
        .iter()
        .find(|h| !typed.is_empty() && h.question.id.starts_with(typed))?;
    Some(format!(
        "'{}' was asked by run '{}', which this machine cannot take back as it stands, so \
         nothing can answer it yet: {}. Once the run is back, the question reopens under a new \
         id: lev interactions",
        h.question.id, h.run_id, h.remedy
    ))
}

/// `err`, or why `typed` cannot be answered when a held run asked it.
async fn or_held(client: &ControlClient, typed: &str, err: anyhow::Error) -> anyhow::Error {
    match held_refusal(typed, &held_questions(client).await) {
        Some(why) => anyhow::anyhow!(why),
        None => err,
    }
}

/// List the interactions the daemon is currently holding.
fn list_interactions(interactions: &[(String, InteractionRequest)], json: bool) {
    if json {
        let open: Vec<OpenInteraction<'_>> = interactions
            .iter()
            .map(|(agent_id, request)| OpenInteraction::new(agent_id, request))
            .collect();
        // Nothing open is an empty array, not a sentence: a caller
        // polling this branches on length, not on prose.
        println!(
            "{}",
            serde_json::to_string_pretty(&open).expect("an interaction listing serializes")
        );
        return;
    }
    if interactions.is_empty() {
        println!("no open interactions");
    } else {
        for (agent_id, req) in interactions {
            println!("{}", format_interaction(agent_id, req));
        }
    }
}

/// Show the one open interaction `typed` names, in full.
fn show_interaction(
    interactions: &[(String, InteractionRequest)],
    typed: &str,
    json: bool,
) -> anyhow::Result<()> {
    let (agent_id, request) = resolve_request_id(typed, interactions)?;
    match json {
        true => println!(
            "{}",
            serde_json::to_string_pretty(&OpenInteraction::new(agent_id, request))
                .expect("an interaction serializes")
        ),
        false => println!("{}", format_interaction_detail(agent_id, request)),
    }
    Ok(())
}

/// `lev interactions`: list the open interactions, or show the one named.
pub async fn interactions(client: &ControlClient, args: &InteractionsArgs) -> anyhow::Result<()> {
    let open = open_interactions(client).await?;
    match &args.request_id {
        None => {
            list_interactions(&open, args.json);
            // A question a held run asked is listed under the ones that can
            // be answered, saying why it cannot be yet. Left out of `--json`,
            // whose caller reads every entry as one it can answer.
            if !args.json {
                for held in held_questions(client).await {
                    println!("{}", format_held(&held));
                }
            }
            Ok(())
        }
        Some(typed) => match show_interaction(&open, typed, args.json) {
            Ok(()) => Ok(()),
            Err(e) => Err(or_held(client, typed, e).await),
        },
    }
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
fn resolve_request_id<'a>(
    typed: &str,
    open: &'a [(String, InteractionRequest)],
) -> anyhow::Result<(&'a str, &'a InteractionRequest)> {
    if typed.is_empty() {
        bail!("name an interaction; `lev interactions` lists the open ones");
    }
    if let Some((agent_id, req)) = open.iter().find(|(_, req)| req.id == typed) {
        return Ok((agent_id, req));
    }
    let named: Vec<&(String, InteractionRequest)> = open
        .iter()
        .filter(|(_, req)| req.id.starts_with(typed))
        .collect();
    match named.as_slice() {
        [] => bail!("no such open interaction"),
        [(agent_id, req)] => Ok((agent_id, req)),
        several => {
            let candidates = several
                .iter()
                .map(|(agent_id, req)| format!("  {}", interaction_headline(agent_id, req)))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "'{typed}' is the start of {} open interactions; give enough of an id to \
                 name just one:\n{candidates}",
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
    let attached = crate::commands::run::attach::attach_all(&args.attach, &cwd)?;
    let open = open_interactions(client).await?;
    let (_, request) = match resolve_request_id(typed, &open) {
        Ok(found) => found,
        Err(e) => return Err(or_held(client, typed, e).await),
    };
    let request_id = request.id.clone();
    // The daemon checks the answer too; checked here as well so the refusal
    // can say what answers the question.
    let response = build_response(request, args)
        .and_then(|response| {
            leviath_core::interaction::check_answer(request, &response).map(|()| response)
        })
        .map_err(|why| {
            anyhow::anyhow!(
                "{why}; nothing was answered. Answer with: {}",
                how_to_answer(request)
            )
        })?;
    let response = attach_answer(response, attached, &cwd)?;
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

/// `lev respond`: answer a pending interaction.
pub async fn respond(client: &ControlClient, args: &RespondArgs) -> anyhow::Result<()> {
    check_one_answer(args)?;
    check_flags(args)?;
    answer_interaction(client, args, &args.request_id).await
}

#[cfg(test)]
#[path = "ctl_tests.rs"]
mod tests;
