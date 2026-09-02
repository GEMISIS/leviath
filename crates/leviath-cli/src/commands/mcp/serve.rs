//! `lev mcp serve` - Leviath as an MCP server over stdio.
//!
//! A host agent (Claude Code, Grok Build, Codex, Gemini, Hermes) launches this
//! as a child process and speaks JSON-RPC 2.0 to it, one compact object per
//! line, on stdin and stdout. Every tool it offers is a thin bridge to the
//! shared-world daemon over the control socket, or a read of a run's record on
//! disk: this process runs no agent of its own, exactly like `lev run` and
//! `lev agent-client`.
//!
//! ## Shape
//!
//! The read loop never blocks on a tool. Every `tools/call` runs in its own
//! task and its reply goes through one channel to a single writer task, so
//! `ping`, `status` and a second `run` are answered while a `run` waits on a
//! multi-stage agent. `notifications/cancelled` aborts the *waiting* task and
//! nothing else: every host-side timeout arrives as that notification, and a
//! healthy run must not die because a host got impatient. The `cancel` tool
//! is the explicit path.
//!
//! The daemon is started lazily, by the first call that needs it, through the
//! injected [`DaemonReady`] factory; only success is remembered, so a cold
//! start that missed its window is retried on the next call rather than
//! poisoning the host's whole session, and the memory is dropped again when
//! the daemon stops answering (`lev daemon stop` under a live host session),
//! so the next call that needs it starts it afresh. `initialize`,
//! `tools/list`, `ping`, `list_agents`, `list_tools` and `install_tool` never
//! touch it; `status` and `result` read the run's record first and only ask
//! the daemon about a run that is still going.
//!
//! A `tools/call` whose id is still in flight is refused with `-32600`
//! (duplicate request id) and the first call is left alone: two calls under
//! one id would be two replies the host cannot tell apart, and the first is
//! the one it is waiting on.
//!
//! stdout is the protocol channel. Nothing here prints; diagnostics go to
//! `tracing` (stderr).
//!
//! The protocol loop is [`serve_over`], generic over its reader and writer at
//! the boundary and erased to trait objects inside, so the whole thing is
//! driven in tests over an in-memory duplex against a fake daemon.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use clap::Args;
use leviath_agent_client::{JsonRpcMessage, error_codes};
use leviath_mcp::server::{self as wire, ServerTool};
use leviath_runtime::control_socket::ControlClient;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

use super::serve_tools::{CallOutcome, call_tool, tool_table};
use crate::daemon::wait::WaitTiming;

/// Arguments for `lev mcp serve`.
#[derive(Args, Debug, Clone)]
pub struct McpServeArgs {
    /// Runs started from the host default to unattended (`yolo`): every tool
    /// call is approved and the agent's own prompts are answered, because the
    /// host model is not a person at a terminal. This flips the default so
    /// runs stop and ask instead; the host then answers with the `respond`
    /// tool. A call's own `yolo` argument overrides either default.
    #[arg(long)]
    pub attended: bool,

    /// Allow a tool outright in every run this server starts (repeatable).
    #[arg(long, value_name = "TOOL")]
    pub allow: Vec<String>,

    /// The agent a `run` call uses when it names none.
    #[arg(long, default_value = "orchestrator", value_name = "NAME")]
    pub default_agent: String,

    /// The working directory for runs whose call omits `workdir` (default: the
    /// host's project directory when it says which, else where this server
    /// was launched).
    #[arg(long, value_name = "DIR")]
    pub workdir: Option<PathBuf>,
}

impl Default for McpServeArgs {
    fn default() -> Self {
        Self {
            attended: false,
            allow: Vec::new(),
            default_agent: "orchestrator".to_string(),
            workdir: None,
        }
    }
}

/// Starts the daemon if it is not running, or says why it could not.
///
/// A factory rather than a future: it is called again after a failure, so a
/// cold start that missed its readiness window is retried on the next call
/// instead of failing every call for the rest of the host's session.
pub type DaemonReady =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>;

/// Everything about the machine the server runs on, injected so the loop is
/// tested against a temp directory and a fake daemon.
pub struct McpServeEnv {
    /// Where run records live (`runstate::runs_dir()` in production).
    pub runs_dir: PathBuf,
    /// The working directory for runs when neither the call nor `--workdir`
    /// names one; also where `list_agents` looks for an `agent.leviath`.
    pub default_cwd: String,
    /// The global Rhai tools directory, `None` when no home resolves.
    pub tools_dir: Option<PathBuf>,
    /// The installed blueprints directory, `None` when no home resolves.
    pub agents_dir: Option<PathBuf>,
    /// The user's home directory, for the workdir guard.
    pub home: Option<PathBuf>,
    /// `[security] allowed_workdirs`: directories the guard does not question.
    pub allowed_workdirs: Vec<String>,
    /// How to make sure the daemon is up; see [`DaemonReady`].
    pub daemon_ready: DaemonReady,
}

/// What the host model is told about this server at `initialize`. Claude Code
/// shows it as "MCP Server Instructions"; other hosts inject it likewise. It is
/// the one steering channel every host renders, so the trigger words and the
/// timeout rule live here as well as in the skill.
pub const INSTRUCTIONS: &str = "Leviath is an agent runtime. When the user says 'leviath' (or \
    'levaith', 'lev run', 'use leviath to ...'), delegate the task with the `run` tool instead of \
    doing it yourself or spawning a subagent. Pass an absolute `workdir`. `run` waits for the run \
    and returns its final output; a host timeout only stops the waiting, the run continues: use \
    `list_runs` to find its run_id, then `wait`, `status` or `cancel`. If a result says \
    waiting_input, answer with `respond` (request_id is in the result) then call `wait`. \
    Mechanics belong in Rhai tools (`install_tool`, `list_tools`); judgement stays in the model.";

/// The most text one tool result carries. Hosts cap what the model sees (Grok
/// Build at 20 kB, Claude Code at 25k tokens) and cut the tail silently; this
/// cuts first and says so, pointing at `result` for the rest.
pub const MCP_TEXT_CAP: usize = 48 * 1024;

/// The clocks the server keeps, injectable so tests do not wait on them.
#[derive(Debug, Clone)]
pub(crate) struct ServeTiming {
    /// How often a waiting `run`/`wait` call emits a progress heartbeat, when
    /// the call carried a progress token.
    pub heartbeat: Duration,
    /// The wait loop's own clocks.
    pub wait: WaitTiming,
}

impl Default for ServeTiming {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(15),
            wait: WaitTiming::default(),
        }
    }
}

/// The reader half, erased to a single trait-object type.
type BoxReader = Pin<Box<dyn AsyncBufRead + Send>>;
/// The writer half, erased to a single trait-object type.
type BoxWriter = Pin<Box<dyn AsyncWrite + Send>>;
/// The one channel every reply and notification goes out through.
type Outbound = mpsc::UnboundedSender<JsonRpcMessage>;

/// The protocol server. Generic over transport at the boundary, then erased
/// to trait objects internally so the state machine has one monomorphization
/// whatever the tests hand it. Returns `Ok(())` when the host closes stdin.
pub async fn serve_over<R, W>(
    reader: R,
    writer: W,
    control: ControlClient,
    args: McpServeArgs,
    env: McpServeEnv,
) -> anyhow::Result<()>
where
    R: AsyncBufRead + Send + 'static,
    W: AsyncWrite + Send + 'static,
{
    serve_over_with(
        Box::pin(reader),
        Box::pin(writer),
        control,
        args,
        env,
        ServeTiming::default(),
    )
    .await
}

/// [`serve_over`] with its clocks injected.
pub(crate) async fn serve_over_with(
    mut reader: BoxReader,
    writer: BoxWriter,
    control: ControlClient,
    args: McpServeArgs,
    env: McpServeEnv,
    timing: ServeTiming,
) -> anyhow::Result<()> {
    let (out, inbox) = mpsc::unbounded_channel();
    let io_dead = Arc::new(Notify::new());
    let writer = tokio::spawn(write_loop(writer, inbox, io_dead.clone()));
    let mut server = Server {
        shared: Arc::new(Shared::new(control, args, env, timing)),
        out,
        in_flight: HashMap::new(),
    };
    server.run(&mut reader, &io_dead).await;
    server.shutdown().await;
    let _ = writer.await;
    Ok(())
}

/// The writer task: one line per message, in order, until the senders are
/// gone or the host stops reading. A failed write means the host is gone, so
/// it flags that and stops rather than propagating an error per call site.
async fn write_loop(
    mut writer: BoxWriter,
    mut inbox: mpsc::UnboundedReceiver<JsonRpcMessage>,
    io_dead: Arc<Notify>,
) {
    while let Some(msg) = inbox.recv().await {
        let mut line = serde_json::to_string(&msg).expect("JsonRpcMessage always serializes");
        line.push('\n');
        let ok = writer.write_all(line.as_bytes()).await.is_ok() && writer.flush().await.is_ok();
        if !ok {
            io_dead.notify_one();
            return;
        }
    }
}

/// What every tool call can reach: the daemon, the arguments, the machine,
/// and the once-only work done at start.
pub(super) struct Shared {
    /// The daemon's control socket.
    pub(super) control: ControlClient,
    /// The server's command line.
    pub(super) args: McpServeArgs,
    /// The machine.
    pub(super) env: McpServeEnv,
    /// The clocks.
    pub(super) timing: ServeTiming,
    /// The names a Rhai tool may not take: built-in and sub-agent tools.
    pub(super) reserved: Vec<String>,
    /// The tool table, built once.
    pub(super) tools: Vec<ServerTool>,
    /// Whether the daemon has been confirmed up. Only success is stored, and
    /// it counts only while the control link still reaches a daemon: a
    /// failed attempt leaves it unset for the next call to try again, and a
    /// daemon that has since stopped answering is started again. An async
    /// mutex, held across the factory's future, so concurrent first calls
    /// start one daemon between them.
    ready: tokio::sync::Mutex<bool>,
}

impl Shared {
    fn new(
        control: ControlClient,
        args: McpServeArgs,
        env: McpServeEnv,
        timing: ServeTiming,
    ) -> Self {
        // Mirrors `tool_inventory`: the built-ins for a workdir plus the
        // sub-agent tools the runtime handles.
        let builtins = leviath_tools::BuiltinTools::new(leviath_tools::ToolContext::new(
            PathBuf::from(&env.default_cwd),
        ));
        let reserved = builtins
            .names()
            .into_iter()
            .chain(leviath_tools::BuiltinTools::subagent_tool_names())
            .collect();
        Self {
            control,
            args,
            env,
            timing,
            reserved,
            tools: tool_table(),
            ready: tokio::sync::Mutex::new(false),
        }
    }

    /// Make sure the daemon is up, starting it on the first call that needs
    /// it and again after it has gone away. An `Err` is returned to the
    /// caller as text and not remembered.
    ///
    /// `ControlClient::link` is refreshed by every request and subscription,
    /// so a daemon stopped since the last success reads as unreachable here
    /// once one call has failed against it, and the factory runs again.
    pub(super) async fn daemon_ready(&self) -> Result<(), String> {
        let mut ready = self.ready.lock().await;
        if *ready && self.control.link().reachable {
            return Ok(());
        }
        (self.env.daemon_ready)().await?;
        *ready = true;
        Ok(())
    }
}

/// A `tools/call` still running in its own task.
struct InFlight {
    task: JoinHandle<()>,
    /// The client's `_meta.progressToken`, kept for the cancel log line.
    progress_token: Option<Value>,
    /// The run the call started, once it has: named in the cancel log line so
    /// someone reading it can find the run the host walked away from.
    run_id: Arc<Mutex<Option<String>>>,
}

/// The parsed shape of a `tools/call`.
struct ToolCall {
    name: String,
    arguments: Value,
    progress_token: Option<Value>,
}

/// Progress notifications for one call: silent without the client's token
/// (a token the client did not mint is logged as an error by its SDK and does
/// not reset its idle timer), strictly increasing with it.
#[derive(Clone)]
pub(super) struct Progress {
    out: Outbound,
    token: Option<Value>,
    counter: Arc<AtomicU64>,
}

impl Progress {
    /// Emit one `notifications/progress`, when there is a token to put on it.
    pub(super) fn emit(&self, message: &str) {
        let Some(token) = &self.token else {
            return;
        };
        let progress = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.out.send(JsonRpcMessage::notification(
            "notifications/progress",
            &wire::progress_params(token, progress, message),
        ));
    }
}

/// The protocol server: the read loop and the in-flight table.
struct Server {
    shared: Arc<Shared>,
    out: Outbound,
    in_flight: HashMap<Value, InFlight>,
}

impl Server {
    /// The read/dispatch loop. Returns when the host closes stdin or stops
    /// reading stdout.
    async fn run(&mut self, reader: &mut BoxReader, io_dead: &Notify) {
        loop {
            tokio::select! {
                biased;
                _ = io_dead.notified() => break,
                line = read_line(reader) => {
                    let Some(line) = line else {
                        break; // EOF or a read error - the host is gone
                    };
                    self.handle_line(line.trim());
                }
            }
        }
    }

    /// Route one line.
    fn handle_line(&mut self, line: &str) {
        if line.is_empty() {
            return; // hosts may emit blank keep-alive lines
        }
        let Ok(msg) = serde_json::from_str::<JsonRpcMessage>(line) else {
            self.send(JsonRpcMessage::error_response(
                Value::Null,
                error_codes::PARSE_ERROR,
                "invalid JSON",
            ));
            return;
        };
        self.in_flight.retain(|_, call| !call.task.is_finished());
        match (msg.method.as_deref(), msg.id) {
            // ── Requests (have an id) ──
            (Some("initialize"), Some(id)) => {
                let client_version = msg
                    .params
                    .as_ref()
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(Value::as_str);
                self.send(JsonRpcMessage::response(
                    id,
                    &wire::initialize_result(client_version, INSTRUCTIONS),
                ));
            }
            (Some("tools/list"), Some(id)) => {
                self.send(JsonRpcMessage::response(
                    id,
                    &wire::tool_list_result(&self.shared.tools),
                ));
            }
            (Some("tools/call"), Some(id)) => self.on_tools_call(id, msg.params),
            (Some("ping"), Some(id)) => {
                self.send(JsonRpcMessage::response(id, &wire::ping_result()));
            }
            (Some(other), Some(id)) => {
                // Includes `server/discover`, so a client of the stateless
                // revision falls back to the `initialize` handshake.
                let (code, message) = wire::method_not_found(other);
                self.send(JsonRpcMessage::error_response(id, code, message));
            }
            // ── Notifications (no id) ──
            (Some("notifications/cancelled"), None) => self.on_cancelled(msg.params),
            // `notifications/initialized`, unknown notifications, and a stray
            // response with neither method nor id are all ignored.
            _ => {}
        }
    }

    /// Start a tool call in its own task; its reply arrives on the channel.
    ///
    /// An id still in flight is refused, and the call under it left alone:
    /// replacing the entry would detach the first task (dropping a
    /// `JoinHandle` aborts nothing) and leave two replies racing for one id.
    fn on_tools_call(&mut self, id: Value, params: Option<Value>) {
        if self.in_flight.contains_key(&id) {
            self.send(JsonRpcMessage::error_response(
                id,
                error_codes::INVALID_REQUEST,
                "duplicate request id: a call with this id is still in flight",
            ));
            return;
        }
        let call = match parse_call(params) {
            Ok(call) => call,
            Err(message) => {
                self.send(JsonRpcMessage::error_response(
                    id,
                    error_codes::INVALID_PARAMS,
                    message,
                ));
                return;
            }
        };
        let run_id = Arc::new(Mutex::new(None));
        let progress = Progress {
            out: self.out.clone(),
            token: call.progress_token.clone(),
            counter: Arc::new(AtomicU64::new(0)),
        };
        let shared = self.shared.clone();
        let out = self.out.clone();
        let reply_id = id.clone();
        let run_slot = run_id.clone();
        let task = tokio::spawn(async move {
            let outcome =
                call_tool(&shared, &call.name, call.arguments, &progress, &run_slot).await;
            let reply = match outcome {
                CallOutcome::Result(result) => JsonRpcMessage::response(reply_id, &result),
                CallOutcome::InvalidParams(message) => {
                    JsonRpcMessage::error_response(reply_id, error_codes::INVALID_PARAMS, message)
                }
            };
            let _ = out.send(reply);
        });
        self.in_flight.insert(
            id,
            InFlight {
                task,
                progress_token: call.progress_token,
                run_id,
            },
        );
    }

    /// `notifications/cancelled`: stop waiting on the call, answer nothing,
    /// and leave the run alone.
    ///
    /// Every host-side timeout - Claude Code's idle and per-server limits,
    /// Codex's 60 s default, Gemini's 10 min default - arrives as exactly this
    /// notification. Cancelling the run on it would kill a healthy multi-stage
    /// run because a host got impatient, which is the failure the `run`
    /// description promises does not happen. The `cancel` tool is explicit.
    fn on_cancelled(&mut self, params: Option<Value>) {
        let Some(request_id) = params.and_then(|p| p.get("requestId").cloned()) else {
            return;
        };
        let Some(call) = self.in_flight.remove(&request_id) else {
            return;
        };
        call.task.abort();
        let run_id = call
            .run_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let request_id = request_id.to_string();
        let run_id = run_id.unwrap_or_else(|| "(none yet)".to_string());
        let progress_token = call
            .progress_token
            .map(|t| t.to_string())
            .unwrap_or_else(|| "(none)".to_string());
        tracing::info!(
            request_id = %request_id,
            run_id = %run_id,
            progress_token = %progress_token,
            "host cancelled a call; stopped waiting, the run (if any) continues"
        );
    }

    /// Queue one message for the writer task.
    fn send(&self, msg: JsonRpcMessage) {
        let _ = self.out.send(msg);
    }

    /// Stop every in-flight call and let the writer drain what is queued.
    async fn shutdown(self) {
        for (_, call) in self.in_flight {
            call.task.abort();
            let _ = call.task.await;
        }
        // Dropping the last sender ends the writer loop once it has written
        // everything already queued.
        drop(self.out);
    }
}

/// Parse `tools/call` params: `name`, an object `arguments` (absent means
/// empty), and the client's `_meta.progressToken`.
fn parse_call(params: Option<Value>) -> Result<ToolCall, String> {
    let Some(params) = params else {
        return Err("tools/call needs params with a tool `name`".to_string());
    };
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Err("tools/call params need a string `name`".to_string());
    };
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => Value::Object(Default::default()),
        Some(args @ Value::Object(_)) => args.clone(),
        Some(_) => return Err("tools/call `arguments` must be a JSON object".to_string()),
    };
    let progress_token = params
        .get("_meta")
        .and_then(|m| m.get("progressToken"))
        .filter(|t| !t.is_null())
        .cloned();
    Ok(ToolCall {
        name: name.to_string(),
        arguments,
        progress_token,
    })
}

/// Read one newline-terminated line, or `None` at end of stream. A read error
/// is treated as EOF: either way the host is no longer sending.
async fn read_line(reader: &mut BoxReader) -> Option<String> {
    let mut line = String::new();
    match reader.read_line(&mut line).await.unwrap_or(0) {
        0 => None,
        _ => Some(line),
    }
}

#[cfg(test)]
#[path = "serve_tests.rs"]
mod tests;
