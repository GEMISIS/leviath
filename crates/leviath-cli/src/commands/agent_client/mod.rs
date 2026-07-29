//! `lev agent-client` - serve a Leviath agent over the Agent **Client** Protocol.
//!
//! ## Which protocol this is
//!
//! This speaks the [Agent Client Protocol][acp]: JSON-RPC 2.0, newline-delimited,
//! over **stdio**. It is the protocol Zed and Gas City use to drive a headless
//! agent as a child process - `initialize` / `session/new` / `session/prompt` /
//! `session/cancel`, with `session/update` notifications streaming output back.
//!
//! It is **not** the Agent *Communication* Protocol (a REST + SSE API from the
//! BeeAI project). The two share the acronym "ACP" and nothing else. This command
//! is the one that actually integrates Leviath with Gas City.
//!
//! ## How it works
//!
//! `lev agent-client` is a thin front end over the shared-world daemon, exactly
//! like `lev run` / `lev serve` / `lev dash`: it owns no agent world of its own.
//! A `session/prompt` spawns (or, on later prompts, messages) an agent in the
//! daemon over the control socket, then translates the daemon's live
//! [`WorldEvent`] stream and the run's per-stage output into `session/update`
//! notifications until the run finishes or parks.
//!
//! [`WorldEvent`]: leviath_runtime::host::WorldEvent
//!
//! The protocol logic is [`serve_over`], which takes its reader/writer generically
//! and erases them to trait objects internally, so the whole
//! handshake→prompt→stream sequence is driven in tests over an in-memory duplex
//! against a fake daemon - no process, no terminal, no real stdio.
//!
//! [acp]: https://agentclientprotocol.com

mod session;
mod translate;

use std::path::PathBuf;

use clap::Args;
use leviath_agent_client::{
    AgentCapabilities, AgentInfo, ContentBlock, InitializeParams, InitializeResult, JsonRpcMessage,
    PROTOCOL_VERSION, PromptCapabilities, RequestPermissionResult, SessionCancelParams,
    SessionNewParams, SessionNewResult, SessionPromptParams, SessionPromptResult, SessionUpdate,
    SessionUpdateParams, StopReason, error_codes, flatten_prompt, is_permission_request,
    parse_region_markers, permission_request,
};
use leviath_core::interaction::{ApprovalScope, InteractionRequest, InteractionResponse};
use leviath_core::run_meta::RunStatus;
use leviath_runtime::control_socket::{ControlClient, ControlRequest, ControlResponse};
use leviath_runtime::host::WorldEvent;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use self::mapping::{PermissionChoice, interpret_permission};
use self::session::{ResolvedBlueprint, resolve_blueprint, spawn_args};
use self::translate::{StageTail, split_chunks};

/// How often, absent a daemon event, the loop flushes newly-written run output.
const OUTPUT_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// Arguments for `lev agent-client`.
#[derive(Args, Debug, Clone, Default)]
pub struct AgentClientArgs {
    /// Blueprint to serve: an installed agent name, or a path to one. When
    /// omitted, each session's working directory is searched for an
    /// `agent.leviath`.
    #[arg(long)]
    pub agent: Option<String>,

    /// Approve every tool call without prompting (recommended when the host does
    /// not implement `session/request_permission`, e.g. Gas City).
    #[arg(long)]
    pub yolo: bool,

    /// Allow a tool outright (repeatable).
    #[arg(long)]
    pub allow: Vec<String>,

    /// Override the blueprint's max sub-agent tree depth.
    #[arg(long)]
    pub max_depth: Option<usize>,

    /// Refuse the blueprint's `seed = { command = "..." }` regions. Those run a
    /// shell command at spawn - before the first inference, and so before any
    /// approval prompt.
    #[arg(long)]
    pub no_seed_commands: bool,
}

/// The protocol server. Generic over transport at the boundary, then erased to
/// trait objects internally (see the body).
///
/// Reads newline-delimited JSON-RPC messages from `reader`, drives the session
/// state machine, and writes responses/notifications to `writer`. `control`
/// reaches the shared-world daemon; `runs_dir` is the run-state root the output
/// tailer reads from (injected so tests use a temp dir). Returns `Ok(())` when
/// the client closes the input stream (EOF).
pub async fn serve_over<R, W>(
    reader: R,
    writer: W,
    control: ControlClient,
    args: AgentClientArgs,
    runs_dir: PathBuf,
    default_cwd: String,
) -> anyhow::Result<()>
where
    R: AsyncBufRead + Send + 'static,
    W: AsyncWrite + Send + 'static,
{
    // Erase the reader/writer to trait objects so the (large, multi-branch)
    // `Server` state machine has exactly ONE monomorphization regardless of the
    // concrete transport. This is the same technique `leviath-cli`'s `serve`
    // module uses on its shutdown future: without it, every distinct test
    // reader/writer type spawns an unused `Server` instantiation whose
    // never-called methods read as uncovered regions under the coverage gate.
    let mut reader: BoxReader = Box::pin(reader);
    let mut server = Server {
        control,
        args,
        runs_dir,
        writer: Box::pin(writer),
        caps_present: false,
        session: None,
        next_request_id: 0,
        io_alive: true,
        default_cwd,
    };
    server.run(&mut reader).await;
    Ok(())
}

/// The reader half, erased to a single trait-object type.
type BoxReader = std::pin::Pin<Box<dyn AsyncBufRead + Send>>;
/// The writer half, erased to a single trait-object type.
type BoxWriter = std::pin::Pin<Box<dyn AsyncWrite + Send>>;

/// The active session's mutable state. Gas City and Zed drive one session per
/// process, so a single slot suffices; a fresh `session/new` replaces it.
struct ActiveSession {
    /// The protocol session id we minted.
    session_id: String,
    /// The blueprint this session runs.
    blueprint: ResolvedBlueprint,
    /// The session's working directory.
    cwd: String,
    /// The daemon run id, once the first prompt has spawned it.
    run_id: Option<String>,
}

/// The protocol server: transport, daemon handle, and session state.
struct Server {
    control: ControlClient,
    args: AgentClientArgs,
    runs_dir: PathBuf,
    writer: BoxWriter,
    /// Whether the client advertised any capabilities at `initialize`. Hosts that
    /// implement the client-side methods (and so can answer an agent-initiated
    /// `session/request_permission`) send them; Gas City sends none, so tool
    /// approvals are surfaced as output and parked instead of deadlocking on a
    /// request the host will never answer.
    caps_present: bool,
    session: Option<ActiveSession>,
    /// Monotonic id source for agent→client requests.
    next_request_id: i64,
    /// Working directory to use when a `session/new` omits (or empties) `cwd` -
    /// the directory `lev agent-client` was launched from. Without this the
    /// agent's workdir was an empty string, so it ran in the daemon's directory
    /// rather than the caller's.
    default_cwd: String,
    /// Whether the output stream is still writable. Output is best-effort: a
    /// failed write means the client is gone, so this flips to `false` and the
    /// server winds down rather than propagating an error per write site (which
    /// mirrors the WebSocket server's `send(...).is_err()` handling).
    io_alive: bool,
}

/// The outcome of getting a run going for a prompt turn.
enum RunStart {
    /// The run is live under this id; stream it.
    Ready(String),
    /// A follow-up message could not be delivered (the agent already finished).
    MessageUndeliverable,
    /// The daemon refused to spawn the run, or was unreachable.
    SpawnFailed,
}

/// What became of an interaction raised mid-turn.
enum InteractionOutcome {
    /// The interaction was resolved in-turn (via `session/request_permission`),
    /// or the client cannot answer it and Leviath will handle it out of band -
    /// either way the turn keeps streaming until the run reaches a done state.
    Continue,
    /// The interaction was surfaced as output and the turn ends now with this
    /// reason, leaving the run parked for the client's next prompt. Used only for
    /// clients that drive their own conversation (they advertised capabilities);
    /// the client re-prompts with the answer.
    Park(StopReason),
}

impl Server {
    /// The top-level read/dispatch loop. Returns when the client closes stdin or
    /// the output stream breaks.
    async fn run(&mut self, reader: &mut BoxReader) {
        while self.io_alive {
            let Some(line) = read_line(reader).await else {
                break; // EOF or a read error - the client is gone
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue; // hosts may emit blank keep-alive lines
            }
            match serde_json::from_str::<JsonRpcMessage>(trimmed) {
                Ok(msg) => self.dispatch(reader, msg).await,
                Err(_) => {
                    self.write(&JsonRpcMessage::error_response(
                        serde_json::Value::Null,
                        error_codes::PARSE_ERROR,
                        "invalid JSON",
                    ))
                    .await;
                }
            }
        }
    }

    /// Route one parsed message.
    async fn dispatch(&mut self, reader: &mut BoxReader, msg: JsonRpcMessage) {
        match (msg.method.as_deref(), msg.id.clone()) {
            // ── Requests (have an id) ──
            (Some("initialize"), Some(id)) => self.on_initialize(id, msg.params).await,
            (Some("session/new"), Some(id)) => self.on_session_new(id, msg.params).await,
            (Some("session/prompt"), Some(id)) => {
                self.on_session_prompt(reader, id, msg.params).await
            }
            (Some(_other), Some(id)) => {
                self.write(&JsonRpcMessage::error_response(
                    id,
                    error_codes::METHOD_NOT_FOUND,
                    "method not supported",
                ))
                .await
            }
            // ── Notifications (no id) ──
            (Some("session/cancel"), None) => self.on_cancel_notification(msg.params).await,
            // Any other notification (`initialized`, unknown) is ignored, as is a
            // stray response with neither method nor id.
            _ => {}
        }
    }

    /// `initialize`: advertise this agent's identity and capabilities, and note
    /// whether the client advertised its own.
    async fn on_initialize(&mut self, id: serde_json::Value, params: Option<serde_json::Value>) {
        let params: InitializeParams = params
            .and_then(|p| serde_json::from_value(p).ok())
            .unwrap_or_default();
        self.caps_present = params.client_capabilities.is_some();
        let result = InitializeResult {
            protocol_version: PROTOCOL_VERSION,
            agent_capabilities: AgentCapabilities {
                load_session: false,
                prompt_capabilities: PromptCapabilities {
                    image: false,
                    audio: false,
                    embedded_context: true,
                },
            },
            agent_info: AgentInfo {
                name: "leviath".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            auth_methods: vec![],
        };
        self.write(&JsonRpcMessage::response(id, &result)).await;
    }

    /// `session/new`: resolve the blueprint and open a session. No run is spawned
    /// yet - that waits for the first prompt.
    async fn on_session_new(&mut self, id: serde_json::Value, params: Option<serde_json::Value>) {
        let params: SessionNewParams = params
            .and_then(|p| serde_json::from_value(p).ok())
            .unwrap_or_default();
        if !params.mcp_servers.is_empty() {
            // Leviath blueprints declare their own MCP servers; client-supplied
            // ones are captured for visibility but not injected (see module docs).
            let ignored = params.mcp_servers.len();
            tracing::info!(
                mcp_server_count = ignored,
                "session/new supplied MCP servers; ignoring in favour of the blueprint's own"
            );
        }
        // An absent/empty `cwd` falls back to the directory `lev agent-client`
        // was launched from, so the agent's tools operate there rather than in
        // the daemon's working directory.
        let cwd = if params.cwd.trim().is_empty() {
            self.default_cwd.clone()
        } else {
            params.cwd
        };
        match resolve_blueprint(self.args.agent.as_deref(), &cwd) {
            Ok(blueprint) => {
                let session_id = new_session_id(&blueprint.agent_name);
                self.session = Some(ActiveSession {
                    session_id: session_id.clone(),
                    blueprint,
                    cwd,
                    run_id: None,
                });
                self.write(&JsonRpcMessage::response(
                    id,
                    &SessionNewResult { session_id },
                ))
                .await;
            }
            Err(e) => {
                self.write(&JsonRpcMessage::error_response(
                    id,
                    error_codes::INVALID_PARAMS,
                    format!("no blueprint for this session: {e}"),
                ))
                .await;
            }
        }
    }

    /// `session/prompt`: run one prompt turn end to end and report its stop reason.
    async fn on_session_prompt(
        &mut self,
        reader: &mut BoxReader,
        id: serde_json::Value,
        params: Option<serde_json::Value>,
    ) {
        if self.session.is_none() {
            self.write(&JsonRpcMessage::error_response(
                id,
                error_codes::INVALID_REQUEST,
                "no active session; call session/new first",
            ))
            .await;
            return;
        }
        let params: SessionPromptParams = params
            .and_then(|p| serde_json::from_value(p).ok())
            .unwrap_or_default();
        let text = flatten_prompt(&params.prompt);
        if text.is_empty() {
            self.write(&JsonRpcMessage::error_response(
                id,
                error_codes::INVALID_PARAMS,
                "prompt has no usable text content",
            ))
            .await;
            return;
        }
        // Parse `---region:<name>---` markers; with none, the whole text is the
        // `task` region (back-compat).
        let regions = parse_region_markers(&text);
        let task = regions.get("task").cloned().unwrap_or_default();
        let stop_reason = self.run_turn(reader, task, regions).await;
        self.write(&JsonRpcMessage::response(
            id,
            &SessionPromptResult { stop_reason },
        ))
        .await;
    }

    /// `session/cancel` notification arriving between turns: cancel the session's
    /// run if one exists.
    async fn on_cancel_notification(&mut self, params: Option<serde_json::Value>) {
        let _: SessionCancelParams = params
            .and_then(|p| serde_json::from_value(p).ok())
            .unwrap_or_default();
        if let Some(run_id) = self.session.as_ref().and_then(|s| s.run_id.clone()) {
            let _ = self
                .control
                .request(&ControlRequest::Cancel { run_id })
                .await;
        }
    }

    /// Drive one prompt turn: spawn (or message) the agent, then translate the
    /// daemon's events and the run's output until it goes terminal or parks.
    async fn run_turn(
        &mut self,
        reader: &mut BoxReader,
        task: String,
        regions: std::collections::HashMap<String, String>,
    ) -> StopReason {
        // Subscribe before spawning so no event between spawn and subscribe is
        // missed. An unreachable daemon ends the turn as a refusal.
        let Ok(mut stream) = self.control.subscribe().await else {
            return StopReason::Refusal;
        };

        let session_id = self
            .session
            .as_ref()
            .expect("session present")
            .session_id
            .clone();
        let run_id = match self.start_run(task, regions).await {
            RunStart::Ready(run_id) => run_id,
            // The agent already finished and won't take another message - the
            // turn is simply over, not a failure.
            RunStart::MessageUndeliverable => return StopReason::EndTurn,
            // The daemon refused to create the run, or was unreachable.
            RunStart::SpawnFailed => return StopReason::Refusal,
        };

        let mut tail = StageTail::new();
        while self.io_alive {
            tokio::select! {
                biased;
                event = stream.next() => {
                    let Some(event) = event else {
                        // The daemon closed the stream (restart); end the turn
                        // with whatever output we have.
                        self.flush_output(&session_id, &mut tail, &run_id).await;
                        return StopReason::EndTurn;
                    };
                    if world_event_run_id(&event) != run_id {
                        continue; // another run in the shared world
                    }
                    self.flush_output(&session_id, &mut tail, &run_id).await;
                    match event {
                        WorldEvent::Completed { status, .. } => {
                            return leviath_agent_client::stop_reason_for_label(&status);
                        }
                        WorldEvent::Context { total_tokens, max_tokens, .. } => {
                            self.emit_usage(&session_id, total_tokens, max_tokens).await;
                        }
                        WorldEvent::Interaction { request, .. } => {
                            match self.handle_interaction(reader, &session_id, &run_id, request).await {
                                InteractionOutcome::Continue => {}
                                InteractionOutcome::Park(reason) => return reason,
                            }
                        }
                        // Status / Tokens / Spawned: the output flush above is all
                        // that's needed.
                        _ => {}
                    }
                    // A run can reach a done state without a `Completed` event -
                    // `CompleteInteractive` stays live for follow-up, so it emits
                    // no terminal event. Consult the persisted run status after
                    // every event so the turn ends when (and only when) the run is
                    // genuinely finished, never while it is merely `WaitingInput`
                    // on an interaction Leviath is handling out of band.
                    if let Some(reason) = self.run_finished(&run_id) {
                        return reason;
                    }
                }
                _ = tokio::time::sleep(OUTPUT_POLL) => {
                    self.flush_output(&session_id, &mut tail, &run_id).await;
                    if let Some(reason) = self.run_finished(&run_id) {
                        return reason;
                    }
                }
                incoming = read_line(reader) => {
                    match incoming {
                        // Stdin closed mid-turn: the client is gone.
                        None => {
                            self.flush_output(&session_id, &mut tail, &run_id).await;
                            return StopReason::EndTurn;
                        }
                        Some(line) => self.handle_midturn_input(&run_id, &line).await,
                    }
                }
            }
        }
        // The output stream broke mid-turn; the client is gone.
        StopReason::EndTurn
    }

    /// Whether the run has reached a state that should end the current turn,
    /// read from its persisted `meta.json` status. Returns the stop reason to
    /// report, or `None` while the run is still starting / running / blocked on
    /// input (`WaitingInput`) - the latter must keep the turn in flight so a
    /// non-interactive client is never told "done" while the agent is actually
    /// waiting on an interaction Leviath is handling out of band.
    fn run_finished(&self, run_id: &str) -> Option<StopReason> {
        let status = read_run_status(&self.runs_dir, run_id)?;
        leviath_agent_client::stop_reason_for(&status)
    }

    /// Spawn the agent on the first prompt, or deliver a message on later ones.
    /// `regions` seeds named caller-input regions on the first (spawning) prompt;
    /// on later prompts the text is delivered as a message and `regions` is unused.
    async fn start_run(
        &mut self,
        task: String,
        regions: std::collections::HashMap<String, String>,
    ) -> RunStart {
        let existing = self
            .session
            .as_ref()
            .expect("session present")
            .run_id
            .clone();
        match existing {
            Some(run_id) => {
                let delivered = matches!(
                    self.control
                        .request(&ControlRequest::Message {
                            agent_id: run_id.clone(),
                            content: task,
                            target_region: None,
                        })
                        .await,
                    Ok(ControlResponse::Ok { ok: true })
                );
                if delivered {
                    RunStart::Ready(run_id)
                } else {
                    RunStart::MessageUndeliverable
                }
            }
            None => {
                let session = self.session.as_ref().expect("session present");
                let spawn =
                    spawn_args(&session.blueprint, &task, &session.cwd, &self.args, regions);
                match self.control.spawn(spawn).await {
                    Ok(ControlResponse::Spawned { run_id }) => {
                        self.session.as_mut().expect("session present").run_id =
                            Some(run_id.clone());
                        RunStart::Ready(run_id)
                    }
                    _ => RunStart::SpawnFailed,
                }
            }
        }
    }

    /// Handle an interaction raised while a turn is streaming.
    ///
    /// The strategy depends on whether the client advertised capabilities at
    /// `initialize` (i.e. whether it implements the client-side protocol methods
    /// and can answer an agent-initiated request):
    ///
    /// - **Capable client + tool approval** → drive it over
    ///   `session/request_permission`, answered in-turn; the run continues.
    /// - **Capable client + any other interaction** → surface the question as
    ///   output and end the turn (`Park`). The client owns the conversation and
    ///   re-prompts with the answer, which arrives as the next `session/prompt` -
    ///   the standard Agent Client Protocol turn boundary.
    /// - **Client without capabilities (e.g. Gas City, which reports interaction
    ///   unsupported)** → surface the question as output and **keep the turn in
    ///   flight** (`Continue`). The run is genuinely blocked, so the turn must not
    ///   report "done"; the human resolves it through Leviath's own surfaces
    ///   (`lev dash` / `lev respond`) and the run then continues to completion.
    async fn handle_interaction(
        &mut self,
        reader: &mut BoxReader,
        session_id: &str,
        run_id: &str,
        request: InteractionRequest,
    ) -> InteractionOutcome {
        if self.caps_present {
            if is_permission_request(&request) {
                return self
                    .request_permission(reader, session_id, run_id, request)
                    .await;
            }
            // A capable client drives its own conversation: surface the question
            // and hand control back so it can re-prompt with the answer.
            self.emit_chunk(session_id, &format!("{}\n", request.prompt))
                .await;
            return InteractionOutcome::Park(StopReason::EndTurn);
        }
        // A client that cannot answer interactions: surface the question and keep
        // the turn alive. Leviath handles the interaction out of band; the turn
        // ends only when the run itself reaches a done state.
        self.emit_chunk(session_id, &format!("{}\n", request.prompt))
            .await;
        InteractionOutcome::Continue
    }

    /// Ask the host to approve a tool call and relay the decision to the daemon.
    async fn request_permission(
        &mut self,
        reader: &mut BoxReader,
        session_id: &str,
        run_id: &str,
        request: InteractionRequest,
    ) -> InteractionOutcome {
        let request_id = serde_json::json!(self.next_id());
        let params = permission_request(session_id, &request);
        self.write(&JsonRpcMessage::request(
            request_id.clone(),
            "session/request_permission",
            &params,
        ))
        .await;

        // Await the matching response. Other inbound messages during the wait are
        // ignored except a cancel, which rejects the call and cancels the run.
        loop {
            let Some(line) = read_line(reader).await else {
                return InteractionOutcome::Park(StopReason::EndTurn);
            };
            let Ok(msg) = serde_json::from_str::<JsonRpcMessage>(line.trim()) else {
                continue;
            };
            if msg.method.as_deref() == Some("session/cancel") {
                let _ = self
                    .control
                    .request(&ControlRequest::Cancel {
                        run_id: run_id.to_string(),
                    })
                    .await;
                self.answer_interaction(&request.id, false, ApprovalScope::Once)
                    .await;
                return InteractionOutcome::Continue;
            }
            if msg.id.as_ref() == Some(&request_id) {
                let choice = msg
                    .result
                    .and_then(|r| serde_json::from_value::<RequestPermissionResult>(r).ok())
                    .map(|r| interpret_permission(&r.outcome))
                    .unwrap_or(PermissionChoice {
                        approved: false,
                        scope: ApprovalScope::Once,
                    });
                self.answer_interaction(&request.id, choice.approved, choice.scope)
                    .await;
                return InteractionOutcome::Continue;
            }
            // Unrelated message; keep waiting.
        }
    }

    /// Relay an approval decision to the daemon's interaction hub.
    ///
    /// Takes `&mut self` (though it only reads `self.control`) so the future
    /// stays `Send`: a shared `&Server` would require `Server: Sync`, which the
    /// erased `dyn AsyncWrite` writer is not.
    async fn answer_interaction(&mut self, request_id: &str, approved: bool, scope: ApprovalScope) {
        let response = InteractionResponse {
            request_id: request_id.to_string(),
            value: None,
            choice_index: None,
            approved: Some(approved),
            scope: Some(scope),
        };
        let _ = self
            .control
            .request(&ControlRequest::AnswerInteraction { response })
            .await;
    }

    /// A message received while a turn is in flight. Only `session/cancel` (for
    /// this run) is actionable; everything else is ignored.
    async fn handle_midturn_input(&mut self, run_id: &str, line: &str) {
        let Ok(msg) = serde_json::from_str::<JsonRpcMessage>(line.trim()) else {
            return;
        };
        if msg.method.as_deref() == Some("session/cancel") {
            let _ = self
                .control
                .request(&ControlRequest::Cancel {
                    run_id: run_id.to_string(),
                })
                .await;
        }
    }

    /// Flush any newly-written run output as `agent_message_chunk` notifications,
    /// split into host-safe frames.
    async fn flush_output(&mut self, session_id: &str, tail: &mut StageTail, run_id: &str) {
        let text = tail.pump(&self.runs_dir, run_id);
        for chunk in split_chunks(&text) {
            self.emit_chunk(session_id, chunk).await;
        }
    }

    /// Emit one `agent_message_chunk` update.
    async fn emit_chunk(&mut self, session_id: &str, text: &str) {
        let params = SessionUpdateParams {
            session_id: session_id.to_string(),
            update: SessionUpdate::AgentMessageChunk {
                content: ContentBlock::text(text),
            },
        };
        self.write(&JsonRpcMessage::notification("session/update", &params))
            .await;
    }

    /// Emit one `usage_update` update.
    async fn emit_usage(&mut self, session_id: &str, used: usize, size: usize) {
        let params = SessionUpdateParams {
            session_id: session_id.to_string(),
            update: SessionUpdate::UsageUpdate { used, size },
        };
        self.write(&JsonRpcMessage::notification("session/update", &params))
            .await;
    }

    /// Serialize one message as a single line and flush it. Output is
    /// best-effort: on any write error the client is assumed gone and
    /// [`Server::io_alive`] flips to `false`, winding the server down.
    async fn write(&mut self, msg: &JsonRpcMessage) {
        let mut line = serde_json::to_string(msg).expect("JsonRpcMessage always serializes");
        line.push('\n');
        let ok = self.writer.write_all(line.as_bytes()).await.is_ok()
            && self.writer.flush().await.is_ok();
        if !ok {
            self.io_alive = false;
        }
    }

    /// Next agent→client request id.
    fn next_id(&mut self) -> i64 {
        self.next_request_id += 1;
        self.next_request_id
    }
}

/// Read the persisted `RunStatus` for `run_id` from `<runs_dir>/<run_id>/meta.json`.
///
/// Deserializes into a minimal projection that reads only the `status` field, so
/// it does not depend on the full [`RunMeta`](leviath_core::run_meta::RunMeta)
/// shape and tolerates a partially-written or older metadata file. Returns
/// `None` if the file is missing or unreadable (the run hasn't persisted yet).
fn read_run_status(runs_dir: &std::path::Path, run_id: &str) -> Option<RunStatus> {
    #[derive(serde::Deserialize)]
    struct StatusOnly {
        status: RunStatus,
    }
    let path = runs_dir.join(run_id).join("meta.json");
    let json = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<StatusOnly>(&json)
        .ok()
        .map(|s| s.status)
}

/// The stop reason to report for a run in `status`, or `None` if the run has not
/// finished and the turn should keep streaming.
///
/// `CompleteInteractive` counts as finished: the agent completed its required
/// work and is only idling for optional follow-up, so control returns to the
/// client. `WaitingInput` does **not** - the agent is blocked on an interaction,
/// which is exactly the state that must not be reported as "done".
/// The run id carried by any [`WorldEvent`] variant.
fn world_event_run_id(event: &WorldEvent) -> &str {
    match event {
        WorldEvent::Spawned { run_id, .. }
        | WorldEvent::Status { run_id, .. }
        | WorldEvent::Tokens { run_id, .. }
        | WorldEvent::Context { run_id, .. }
        | WorldEvent::Interaction { run_id, .. }
        | WorldEvent::Completed { run_id, .. }
        | WorldEvent::Log { run_id, .. } => run_id,
    }
}

/// Mint a session id from the agent name - reuses the run-id generator's
/// collision-resistant `<name>-<timestamp>-<suffix>` scheme.
fn new_session_id(agent_name: &str) -> String {
    crate::runstate::new_run_id(agent_name)
}

/// Read one newline-terminated line, or `None` at end of stream.
///
/// A read error is treated the same as EOF (`None`): either way the client is no
/// longer sending, and there is nothing useful to do but wind down. Collapsing
/// the error into `None` via `unwrap_or(0)` also keeps this branch-free for the
/// coverage gate.
async fn read_line(reader: &mut BoxReader) -> Option<String> {
    let mut line = String::new();
    match reader.read_line(&mut line).await.unwrap_or(0) {
        0 => None,
        _ => Some(line),
    }
}

/// Small, pure translations for the prompt loop, kept apart so they unit-test in
/// isolation from the async machinery.
mod mapping {
    use leviath_agent_client::PermissionOutcome;
    use leviath_agent_client::mapping::{
        OPTION_ALLOW_ALWAYS, OPTION_ALLOW_ONCE, OPTION_REJECT_ONCE,
    };
    use leviath_core::interaction::ApprovalScope;

    /// A resolved permission decision to relay to the daemon.
    pub(super) struct PermissionChoice {
        /// Whether the tool call is approved.
        pub(super) approved: bool,
        /// The scope of the decision.
        pub(super) scope: ApprovalScope,
    }

    /// Interpret a host's permission outcome into an approve/deny + scope.
    ///
    /// The three option ids we offer map to allow-once, allow-for-session, and
    /// reject. A `cancelled` outcome, or any option id we did not offer, is
    /// treated as a one-time rejection - the safe default.
    pub(super) fn interpret_permission(outcome: &PermissionOutcome) -> PermissionChoice {
        match outcome {
            PermissionOutcome::Selected { option_id } if option_id == OPTION_ALLOW_ONCE => {
                PermissionChoice {
                    approved: true,
                    scope: ApprovalScope::Once,
                }
            }
            PermissionOutcome::Selected { option_id } if option_id == OPTION_ALLOW_ALWAYS => {
                PermissionChoice {
                    approved: true,
                    scope: ApprovalScope::Session,
                }
            }
            PermissionOutcome::Selected { option_id } if option_id == OPTION_REJECT_ONCE => {
                PermissionChoice {
                    approved: false,
                    scope: ApprovalScope::Once,
                }
            }
            _ => PermissionChoice {
                approved: false,
                scope: ApprovalScope::Once,
            },
        }
    }

    /// The stop reason to report for a run whose `WorldEvent::Completed` carried
    /// `status`. The host emits only the terminal statuses `complete`, `error`,
    /// and `cancelled`.
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn interpret_maps_every_offered_option() {
            let allow_once = interpret_permission(&PermissionOutcome::Selected {
                option_id: OPTION_ALLOW_ONCE.to_string(),
            });
            assert!(allow_once.approved);
            assert_eq!(allow_once.scope, ApprovalScope::Once);

            let allow_always = interpret_permission(&PermissionOutcome::Selected {
                option_id: OPTION_ALLOW_ALWAYS.to_string(),
            });
            assert!(allow_always.approved);
            assert_eq!(allow_always.scope, ApprovalScope::Session);

            let reject = interpret_permission(&PermissionOutcome::Selected {
                option_id: OPTION_REJECT_ONCE.to_string(),
            });
            assert!(!reject.approved);
            assert_eq!(reject.scope, ApprovalScope::Once);
        }

        #[test]
        fn interpret_denies_unknown_option_and_cancellation() {
            let unknown = interpret_permission(&PermissionOutcome::Selected {
                option_id: "made-up".to_string(),
            });
            assert!(!unknown.approved);
            assert_eq!(unknown.scope, ApprovalScope::Once);

            let cancelled = interpret_permission(&PermissionOutcome::Cancelled);
            assert!(!cancelled.approved);
            assert_eq!(cancelled.scope, ApprovalScope::Once);
        }
    }
}

#[cfg(test)]
mod tests;
