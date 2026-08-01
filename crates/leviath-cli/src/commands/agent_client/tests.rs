//! End-to-end tests for the Agent Client Protocol server, driven over an
//! in-memory duplex against a scripted fake daemon.
//!
//! Each test spins up [`serve_over`] on a background task wired to two
//! `tokio::io::duplex` pipes (the server's stdin and stdout) and a
//! [`ScriptedDaemon`] listening on a real control socket in a temp dir. The
//! harness sends JSON-RPC lines and reads the server's replies, so the whole
//! handshake → prompt → stream sequence is exercised without a process,
//! terminal, or real daemon.

use super::*;

use std::sync::Arc;

use leviath_agent_client::PROTOCOL_VERSION;
use leviath_core::interaction::{InteractionKind, InteractionRequest};
use leviath_runtime::control_socket::{ControlClient, bind_control_listener, control_id};
use leviath_runtime::host::WorldEvent;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::task::JoinHandle;

/// The run id the scripted daemon assigns to every spawn, so tests can address
/// its on-disk output.
const RUN_ID: &str = "coder-test-run";

/// The default working directory the harness gives the server, standing in for
/// the directory `lev agent-client` was launched from.
const HARNESS_DEFAULT_CWD: &str = "/harness-launch-dir";

/// Aborts a background task when the harness is dropped.
struct AbortOnDrop(JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A fake shared-world daemon: streams a fixed [`WorldEvent`] script to any
/// `Subscribe`, and answers every other request via a closure.
struct ScriptedDaemon {
    client: ControlClient,
    _dir: tempfile::TempDir,
    _accept: AbortOnDrop,
}

impl ScriptedDaemon {
    fn new(
        events: Vec<WorldEvent>,
        responder: impl Fn(ControlRequest) -> ControlResponse + Send + Sync + 'static,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let id = control_id(dir.path());
        let mut listener = bind_control_listener(&id).unwrap();
        let events = Arc::new(events);
        let responder = Arc::new(responder);
        let accept = tokio::spawn(async move {
            loop {
                // `Ok(None)` is a peer that is not this user; in a test the
                // only connection is our own.
                let Ok(Some(stream)) = listener.accept().await else {
                    break;
                };
                let events = events.clone();
                let responder = responder.clone();
                tokio::spawn(async move {
                    let (read_half, mut write_half) = tokio::io::split(stream);
                    let mut lines = BufReader::new(read_half).lines();
                    let Ok(Some(line)) = lines.next_line().await else {
                        return;
                    };
                    let Ok(req) = serde_json::from_str::<ControlRequest>(&line) else {
                        return;
                    };
                    match req {
                        ControlRequest::Subscribe => {
                            for ev in events.iter() {
                                let mut out = serde_json::to_string(ev).unwrap();
                                out.push('\n');
                                if write_half.write_all(out.as_bytes()).await.is_err() {
                                    return;
                                }
                            }
                            // Hold the connection open so the client's event
                            // stream doesn't hit EOF before it has processed the
                            // terminal event; the task is aborted with the daemon.
                            std::future::pending::<()>().await;
                        }
                        other => {
                            let mut out = serde_json::to_string(&responder(other)).unwrap();
                            out.push('\n');
                            let _ = write_half.write_all(out.as_bytes()).await;
                        }
                    }
                });
            }
        });
        Self {
            client: ScriptedDaemon::client_at(&dir),
            _dir: dir,
            _accept: AbortOnDrop(accept),
        }
    }

    fn client_at(dir: &tempfile::TempDir) -> ControlClient {
        ControlClient::new(control_id(dir.path()))
    }

    fn client(&self) -> ControlClient {
        self.client.clone()
    }
}

/// A running [`serve_over`] plus the pipes to drive it.
struct Harness {
    to_server: DuplexStream,
    from_server: BufReader<DuplexStream>,
    runs_dir: tempfile::TempDir,
    _daemon: ScriptedDaemon,
    _server: AbortOnDrop,
}

impl Harness {
    /// Start a server against `daemon` with the given CLI args.
    fn start(daemon: ScriptedDaemon, args: AgentClientArgs) -> Self {
        let (to_server, server_in) = tokio::io::duplex(64 * 1024);
        let (server_out, from_server) = tokio::io::duplex(1024 * 1024);
        let runs_dir = tempfile::tempdir().unwrap();
        let control = daemon.client();
        let runs_path = runs_dir.path().to_path_buf();
        let server = tokio::spawn(async move {
            let _ = serve_over(
                BufReader::new(server_in),
                server_out,
                control,
                args,
                runs_path,
                HARNESS_DEFAULT_CWD.to_string(),
            )
            .await;
        });
        Self {
            to_server,
            from_server: BufReader::new(from_server),
            runs_dir,
            _daemon: daemon,
            _server: AbortOnDrop(server),
        }
    }

    /// Send one raw line (a newline is appended).
    async fn send(&mut self, line: &str) {
        self.to_server.write_all(line.as_bytes()).await.unwrap();
        self.to_server.write_all(b"\n").await.unwrap();
        self.to_server.flush().await.unwrap();
    }

    /// Close the server's input, so it reaches EOF and returns.
    async fn close_input(&mut self) {
        self.to_server.shutdown().await.unwrap();
    }

    /// Read the next JSON-RPC message the server emits.
    async fn recv(&mut self) -> JsonRpcMessage {
        let mut line = String::new();
        let n = self.from_server.read_line(&mut line).await.unwrap();
        assert_ne!(n, 0, "server closed its output before sending a message");
        serde_json::from_str(line.trim()).unwrap()
    }

    /// Read messages until one satisfies `pred`, returning it (and discarding the
    /// notifications before it).
    async fn recv_until(&mut self, pred: impl Fn(&JsonRpcMessage) -> bool) -> JsonRpcMessage {
        loop {
            let msg = self.recv().await;
            if pred(&msg) {
                return msg;
            }
        }
    }

    /// Write `RUN_ID`'s persisted `meta.json` with the given snake_case status,
    /// simulating what the daemon's persistence lane records (the turn reads this
    /// to decide when the run is genuinely done).
    fn write_meta_status(&self, status: &str) {
        let dir = self.runs_dir.path().join(RUN_ID);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("meta.json"), format!(r#"{{"status":"{status}"}}"#)).unwrap();
    }

    /// Write agent output to `RUN_ID`'s stage `idx` output log under the runs dir.
    fn write_output(&self, idx: usize, text: &str) {
        let path = self
            .runs_dir
            .path()
            .join(RUN_ID)
            .join("stages")
            .join(idx.to_string())
            .join("output.log");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

// ─── Event/response builders ─────────────────────────────────────────────────

fn completed(status: &str) -> WorldEvent {
    WorldEvent::Completed {
        run_id: RUN_ID.to_string(),
        agent_id: RUN_ID.to_string(),
        status: status.to_string(),
    }
}

fn status_event() -> WorldEvent {
    WorldEvent::Status {
        run_id: RUN_ID.to_string(),
        agent_id: RUN_ID.to_string(),
        status: "active".to_string(),
        stage: "implement".to_string(),
        iteration: 1,
        tool_calls: 0,
        accepts_messages: false,
    }
}

fn spawned_event() -> WorldEvent {
    WorldEvent::Spawned {
        run_id: RUN_ID.to_string(),
        agent_id: RUN_ID.to_string(),
        blueprint: "coder".to_string(),
    }
}

fn tokens_event() -> WorldEvent {
    WorldEvent::Tokens {
        run_id: RUN_ID.to_string(),
        agent_id: RUN_ID.to_string(),
        prompt_tokens: 10,
        completion_tokens: 5,
        cached_tokens: 0,
        cache_write_tokens: 0,
    }
}

fn context_event(total: usize, max: usize) -> WorldEvent {
    WorldEvent::Context {
        run_id: RUN_ID.to_string(),
        agent_id: RUN_ID.to_string(),
        total_tokens: total,
        max_tokens: max,
    }
}

fn approval_event() -> WorldEvent {
    WorldEvent::Interaction {
        run_id: RUN_ID.to_string(),
        agent_id: RUN_ID.to_string(),
        request: InteractionRequest {
            id: "appr-1".to_string(),
            kind: InteractionKind::ToolApproval,
            prompt: "Run bash `ls`?".to_string(),
            options: vec![],
            tool_name: Some("bash".to_string()),
            tool_arguments: None,
            required: true,
            stage_name: "implement".to_string(),
            body: None,
            body_format: Default::default(),
        },
    }
}

fn free_text_event() -> WorldEvent {
    WorldEvent::Interaction {
        run_id: RUN_ID.to_string(),
        agent_id: RUN_ID.to_string(),
        request: InteractionRequest::free_text("q1", "What color?", "implement", true),
    }
}

/// A responder that spawns `RUN_ID` and says yes to everything else.
fn spawn_ok(req: ControlRequest) -> ControlResponse {
    match req {
        ControlRequest::Spawn { .. } => ControlResponse::Spawned {
            run_id: RUN_ID.to_string(),
        },
        _ => ControlResponse::Ok { ok: true },
    }
}

/// A blueprint in a `coder` subdir of a temp dir (so its agent name is "coder");
/// returns args pointing `--agent` at it, and the temp root kept alive.
fn blueprint_args() -> (tempfile::TempDir, AgentClientArgs) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("coder");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("agent.leviath"),
        r#"
[agent]
name = "coder"
version = "1.0.0"
description = "test"

[stages.implement]
prompt = "Do it"
"#,
    )
    .unwrap();
    let args = AgentClientArgs {
        agent: Some(dir.to_string_lossy().to_string()),
        yolo: false,
        no_seed_commands: false,
        allow: vec![],
        max_depth: None,
    };
    (root, args)
}

fn is_result(msg: &JsonRpcMessage) -> bool {
    msg.result.is_some() || msg.error.is_some()
}

fn update_kind(msg: &JsonRpcMessage) -> Option<String> {
    if msg.method.as_deref() != Some("session/update") {
        return None;
    }
    msg.params
        .as_ref()?
        .get("update")?
        .get("sessionUpdate")?
        .as_str()
        .map(str::to_string)
}

/// Drive a first prompt to completion: initialize (with or without caps),
/// session/new, session/prompt. Returns the harness for further assertions.
async fn opened_session(daemon: ScriptedDaemon, with_caps: bool) -> (Harness, tempfile::TempDir) {
    let (bp, args) = blueprint_args();
    let mut h = Harness::start(daemon, args);
    let init = if with_caps {
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":true}}}}"#
    } else {
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#
    };
    h.send(init).await;
    let _ = h.recv().await; // initialize result
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp"}}"#)
        .await;
    let _ = h.recv().await; // session/new result
    (h, bp)
}

// ─── output stream breaks ────────────────────────────────────────────────────

/// An `AsyncWrite` that fails every write, to drive the best-effort output
/// path's failure branch.
struct FailingWriter;

impl tokio::io::AsyncWrite for FailingWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Err(std::io::Error::other("write failed")))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// An `AsyncWrite` that succeeds for the first `ok` writes, then fails - to
/// break the stream partway through a session.
struct FailAfter {
    remaining: std::sync::atomic::AtomicUsize,
}

impl tokio::io::AsyncWrite for FailAfter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use std::sync::atomic::Ordering;
        if self.remaining.load(Ordering::SeqCst) == 0 {
            return std::task::Poll::Ready(Err(std::io::Error::other("write failed")));
        }
        self.remaining.fetch_sub(1, Ordering::SeqCst);
        std::task::Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn output_failing_mid_turn_ends_the_turn() {
    // Succeed through `initialize` + `session/new` (2 writes), then fail on the
    // first mid-turn output chunk so the turn's loop exits on the broken stream.
    let daemon = ScriptedDaemon::new(vec![status_event()], spawn_ok);
    let control = daemon.client();
    let runs = tempfile::tempdir().unwrap();
    // Pre-write output so the status event has something to flush.
    let out = runs
        .path()
        .join(RUN_ID)
        .join("stages")
        .join("0")
        .join("output.log");
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    std::fs::write(out, "mid-turn output\n").unwrap();

    let (_bp, args) = blueprint_args();
    let cwd = args.agent.clone().unwrap();
    // Build the session/new line via serde so the cwd path is JSON-escaped -
    // Windows paths contain backslashes that would otherwise be invalid JSON.
    let session_new = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": {"cwd": cwd},
    });
    let script = format!(
        "{}\n{}\n{}\n",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
        session_new,
        r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#,
    );
    let writer = FailAfter {
        remaining: std::sync::atomic::AtomicUsize::new(2),
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        serve_over(
            BufReader::new(std::io::Cursor::new(script.into_bytes())),
            writer,
            control,
            args,
            runs.path().to_path_buf(),
            HARNESS_DEFAULT_CWD.to_string(),
        ),
    )
    .await;
    assert!(result.unwrap().is_ok());
    drop(daemon);
}

#[tokio::test]
async fn a_broken_output_stream_winds_the_server_down() {
    // A single `initialize` whose response write fails must end the loop
    // (io_alive → false) rather than spin, so serve_over returns promptly.
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    let control = daemon.client();
    let runs = tempfile::tempdir().unwrap().path().to_path_buf();
    let input = std::io::Cursor::new(
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n".to_vec(),
    );
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        serve_over(
            BufReader::new(input),
            FailingWriter,
            control,
            AgentClientArgs::default(),
            runs,
            HARNESS_DEFAULT_CWD.to_string(),
        ),
    )
    .await;
    // It returned (didn't hang) and reported success.
    assert!(result.unwrap().is_ok());
    drop(daemon);
}

// ─── initialize ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn initialize_advertises_agent_identity_and_capabilities() {
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#)
        .await;
    let resp = h.recv().await;
    let result = resp.result.unwrap();
    assert_eq!(result["protocolVersion"], PROTOCOL_VERSION);
    assert_eq!(result["agentInfo"]["name"], "leviath");
    assert_eq!(result["agentCapabilities"]["loadSession"], false);
    assert_eq!(
        result["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
        true
    );
    h.close_input().await;
}

#[tokio::test]
async fn initialize_tolerates_absent_params() {
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#)
        .await;
    assert_eq!(
        h.recv().await.result.unwrap()["protocolVersion"],
        PROTOCOL_VERSION
    );
    h.close_input().await;
}

// ─── session/new ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn session_new_opens_a_session_and_returns_its_id() {
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    let (bp, args) = blueprint_args();
    let mut h = Harness::start(daemon, args);
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp"}}"#)
        .await;
    let resp = h.recv().await;
    assert!(
        resp.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .starts_with("coder")
    );
    drop(bp);
    h.close_input().await;
}

#[tokio::test]
async fn empty_cwd_defaults_to_the_launch_directory() {
    // Regression for #80: a `session/new` with an empty `cwd` must give the
    // spawned agent the directory `lev agent-client` was launched from (the
    // harness's default), not an empty workdir that runs in the daemon's dir.
    let captured = Arc::new(std::sync::Mutex::new(None));
    let cap = captured.clone();
    let daemon = ScriptedDaemon::new(vec![completed("complete")], move |req| match req {
        ControlRequest::Spawn { args } => {
            *cap.lock().unwrap() = Some(args.workdir.clone());
            ControlResponse::Spawned {
                run_id: RUN_ID.to_string(),
            }
        }
        _ => ControlResponse::Ok { ok: true },
    });
    let (_bp, args) = blueprint_args();
    let mut h = Harness::start(daemon, args);
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#)
        .await;
    let _ = h.recv().await;
    // Empty cwd → falls back to the launch directory.
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":""}}"#)
        .await;
    let _ = h.recv().await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    let _ = h.recv_until(is_result).await;
    assert_eq!(
        captured.lock().unwrap().as_deref(),
        Some(HARNESS_DEFAULT_CWD)
    );
    h.close_input().await;
}

#[tokio::test]
async fn session_new_logs_and_ignores_supplied_mcp_servers() {
    // Install the always-on tracing subscriber so the `tracing::info!` body for
    // the ignored MCP servers is actually evaluated (and covered).
    crate::test_support::with_tracing(|| {});
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    let (_bp, args) = blueprint_args();
    let mut h = Harness::start(daemon, args);
    h.send(
        r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[{"name":"x","command":"y"}]}}"#,
    )
    .await;
    // Still opens the session - the servers are only logged.
    assert!(h.recv().await.result.is_some());
    h.close_input().await;
}

#[tokio::test]
async fn session_new_errors_when_no_blueprint_resolves() {
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    // No --agent, and the cwd has no blueprint.
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    let empty = tempfile::tempdir().unwrap();
    // serde-build so the (possibly backslash-containing) path is JSON-escaped.
    let msg = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/new",
        "params": {"cwd": empty.path().to_string_lossy()},
    });
    h.send(&msg.to_string()).await;
    let resp = h.recv().await;
    assert_eq!(resp.error.unwrap().code, error_codes::INVALID_PARAMS);
    h.close_input().await;
}

// ─── dispatch edge cases ─────────────────────────────────────────────────────

#[tokio::test]
async fn unknown_method_is_method_not_found() {
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    h.send(r#"{"jsonrpc":"2.0","id":9,"method":"does/not-exist"}"#)
        .await;
    let resp = h.recv().await;
    assert_eq!(resp.id.unwrap(), serde_json::json!(9));
    assert_eq!(resp.error.unwrap().code, error_codes::METHOD_NOT_FOUND);
    h.close_input().await;
}

#[tokio::test]
async fn invalid_json_is_a_parse_error() {
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    h.send("this is not json").await;
    let resp = h.recv().await;
    assert_eq!(resp.error.unwrap().code, error_codes::PARSE_ERROR);
    h.close_input().await;
}

#[tokio::test]
async fn blank_lines_and_bare_notifications_are_ignored() {
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    h.send("").await; // blank
    h.send(r#"{"jsonrpc":"2.0","method":"initialized"}"#).await; // notification
    h.send(r#"{"jsonrpc":"2.0","id":7}"#).await; // bare response, no method
    // The next real request still gets a reply, proving the earlier lines were
    // silently consumed.
    h.send(r#"{"jsonrpc":"2.0","id":8,"method":"initialize"}"#)
        .await;
    assert_eq!(h.recv().await.id.unwrap(), serde_json::json!(8));
    h.close_input().await;
}

// ─── session/prompt: preconditions ───────────────────────────────────────────

#[tokio::test]
async fn prompt_without_a_session_is_an_error() {
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"hi"}]}}"#)
        .await;
    assert_eq!(
        h.recv().await.error.unwrap().code,
        error_codes::INVALID_REQUEST
    );
    h.close_input().await;
}

#[tokio::test]
async fn prompt_with_no_usable_text_is_an_error() {
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"image"}]}}"#)
        .await;
    assert_eq!(
        h.recv().await.error.unwrap().code,
        error_codes::INVALID_PARAMS
    );
    h.close_input().await;
}

// ─── session/prompt: happy path + streaming ──────────────────────────────────

#[tokio::test]
async fn prompt_streams_output_then_ends_the_turn() {
    // Include Spawned and Tokens events too: both just trigger an output flush
    // and are otherwise passed over, exercising those `WorldEvent` arms.
    let daemon = ScriptedDaemon::new(
        vec![
            spawned_event(),
            status_event(),
            tokens_event(),
            completed("complete"),
        ],
        spawn_ok,
    );
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.write_output(0, "working on it\n");
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    // A chunk carrying the run output arrives before the final result.
    let chunk = h
        .recv_until(|m| update_kind(m).as_deref() == Some("agent_message_chunk"))
        .await;
    assert_eq!(
        chunk.params.unwrap()["update"]["content"]["text"],
        "working on it\n"
    );
    let result = h.recv_until(is_result).await;
    assert_eq!(result.result.unwrap()["stopReason"], "end_turn");
    h.close_input().await;
}

#[tokio::test]
async fn prompt_maps_error_completion_to_refusal() {
    let daemon = ScriptedDaemon::new(vec![completed("error")], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    let result = h.recv_until(is_result).await;
    assert_eq!(result.result.unwrap()["stopReason"], "refusal");
    h.close_input().await;
}

#[tokio::test]
async fn prompt_maps_cancelled_completion_to_cancelled() {
    let daemon = ScriptedDaemon::new(vec![completed("cancelled")], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "cancelled"
    );
    h.close_input().await;
}

#[tokio::test]
async fn context_events_become_usage_updates() {
    let daemon = ScriptedDaemon::new(
        vec![context_event(120, 8000), completed("complete")],
        spawn_ok,
    );
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    let usage = h
        .recv_until(|m| update_kind(m).as_deref() == Some("usage_update"))
        .await;
    let update = &usage.params.unwrap()["update"];
    assert_eq!(update["used"], 120);
    assert_eq!(update["size"], 8000);
    h.close_input().await;
}

#[tokio::test]
async fn events_for_other_runs_are_ignored() {
    let mut other = completed("complete");
    if let WorldEvent::Completed { run_id, .. } = &mut other {
        *run_id = "someone-else".to_string();
    }
    // The foreign completion must not end our turn; our own does.
    let daemon = ScriptedDaemon::new(vec![other, completed("complete")], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "end_turn"
    );
    h.close_input().await;
}

#[test]
fn world_event_run_id_reads_the_run_id_from_a_log_event() {
    // A `Log` event carries the run id like every other variant; the pump uses
    // this to filter events to the active run.
    let ev = WorldEvent::Log {
        run_id: "run-log".to_string(),
        agent_id: "a".to_string(),
        line: "some output".to_string(),
    };
    assert_eq!(ev.run_id(), "run-log");
}

// ─── session/prompt: spawn / message failures ────────────────────────────────

#[tokio::test]
async fn a_refused_spawn_ends_the_turn_as_refusal() {
    let daemon = ScriptedDaemon::new(vec![], |req| match req {
        ControlRequest::Spawn { .. } => ControlResponse::Error {
            message: "bad blueprint".to_string(),
        },
        _ => ControlResponse::Ok { ok: true },
    });
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "refusal"
    );
    h.close_input().await;
}

#[tokio::test]
async fn a_second_prompt_is_delivered_as_a_message() {
    let daemon = ScriptedDaemon::new(vec![completed("complete")], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, false).await;
    // First prompt spawns and completes.
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"one"}]}}"#)
        .await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "end_turn"
    );
    // Second prompt: run_id already set, so it goes as a Message (Ok true).
    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"two"}]}}"#)
        .await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "end_turn"
    );
    h.close_input().await;
}

#[tokio::test]
async fn a_second_prompt_that_cannot_be_delivered_ends_the_turn() {
    // Spawn succeeds; Message is rejected (agent not accepting).
    let daemon = ScriptedDaemon::new(vec![completed("complete")], |req| match req {
        ControlRequest::Spawn { .. } => ControlResponse::Spawned {
            run_id: RUN_ID.to_string(),
        },
        ControlRequest::Message { .. } => ControlResponse::Ok { ok: false },
        _ => ControlResponse::Ok { ok: true },
    });
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"one"}]}}"#)
        .await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "end_turn"
    );
    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"two"}]}}"#)
        .await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "end_turn"
    );
    h.close_input().await;
}

// ─── interactions: no client capabilities (Gas City) - keep turn in flight ───

#[tokio::test]
async fn an_interaction_without_capabilities_is_surfaced_but_does_not_end_the_turn() {
    // The interaction is raised, then the run continues and completes. The turn
    // must surface the question AND only end on the completion - never on the
    // interaction itself (which would tell the client "done" prematurely).
    let daemon = ScriptedDaemon::new(vec![free_text_event(), completed("complete")], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    // The question is surfaced as agent output…
    let chunk = h
        .recv_until(|m| update_kind(m).as_deref() == Some("agent_message_chunk"))
        .await;
    assert!(
        chunk.params.unwrap()["update"]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("What color?")
    );
    // …and the turn ends only when the run actually completes.
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "end_turn"
    );
    h.close_input().await;
}

#[tokio::test]
async fn a_tool_approval_without_capabilities_keeps_the_turn_alive_until_done() {
    let daemon = ScriptedDaemon::new(vec![approval_event(), completed("complete")], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "end_turn"
    );
    h.close_input().await;
}

#[tokio::test]
async fn a_run_that_settles_into_complete_interactive_ends_the_turn_via_meta() {
    // CompleteInteractive emits no terminal `Completed` event - the agent stays
    // live for follow-up - so the turn must detect "done" from the persisted run
    // status on the poll tick.
    let daemon = ScriptedDaemon::new(vec![status_event()], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.write_meta_status("complete_interactive");
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "end_turn"
    );
    h.close_input().await;
}

#[tokio::test]
async fn a_done_run_status_is_detected_on_the_poll_tick_with_no_events() {
    // No world events flow at all (subscribe stays open, empty). The run's done
    // state is discovered only by the periodic status poll, exercising the tick
    // branch's completion check.
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.write_meta_status("complete");
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "end_turn"
    );
    h.close_input().await;
}

#[tokio::test]
async fn a_waiting_input_run_status_does_not_end_the_turn() {
    // meta says the run is blocked on input (waiting_input); a status event flows
    // but the turn must NOT end on it. Only the later completion returns.
    let daemon = ScriptedDaemon::new(vec![status_event(), completed("complete")], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.write_meta_status("waiting_input");
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "end_turn"
    );
    h.close_input().await;
}

#[tokio::test]
async fn an_error_run_status_ends_the_turn_as_refusal_via_meta() {
    let daemon = ScriptedDaemon::new(vec![status_event()], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.write_meta_status("error");
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "refusal"
    );
    h.close_input().await;
}

#[tokio::test]
async fn a_cancelled_run_status_ends_the_turn_as_cancelled_via_meta() {
    let daemon = ScriptedDaemon::new(vec![status_event()], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.write_meta_status("cancelled");
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "cancelled"
    );
    h.close_input().await;
}

// ─── interactions: capable client parks non-approval interactions ─────────────

#[tokio::test]
async fn a_capable_client_parks_a_non_approval_interaction() {
    // With capabilities, a free-text question is surfaced and the turn ends so
    // the client can re-prompt with the answer (standard ACP), even with no
    // completion event.
    let daemon = ScriptedDaemon::new(vec![free_text_event()], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, true).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    let chunk = h
        .recv_until(|m| update_kind(m).as_deref() == Some("agent_message_chunk"))
        .await;
    assert!(
        chunk.params.unwrap()["update"]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("What color?")
    );
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "end_turn"
    );
    h.close_input().await;
}

// ─── interactions: permission (client has capabilities) ──────────────────────

#[tokio::test]
async fn a_tool_approval_with_capabilities_becomes_a_permission_request() {
    let daemon = ScriptedDaemon::new(vec![approval_event(), completed("complete")], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, true).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    // The agent asks the host to approve the call.
    let perm = h
        .recv_until(|m| m.method.as_deref() == Some("session/request_permission"))
        .await;
    let perm_id = perm.id.clone().unwrap();
    assert_eq!(perm.params.unwrap()["toolCall"]["title"], "bash");
    // Approve it; the run then completes.
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{},"result":{{"outcome":{{"outcome":"selected","optionId":"allow-once"}}}}}}"#,
        perm_id
    ))
    .await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "end_turn"
    );
    h.close_input().await;
}

#[tokio::test]
async fn a_permission_response_with_no_result_denies_and_continues() {
    let daemon = ScriptedDaemon::new(vec![approval_event(), completed("complete")], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, true).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    let perm = h
        .recv_until(|m| m.method.as_deref() == Some("session/request_permission"))
        .await;
    // A response carrying no `result` (e.g. an error the host returned) → deny.
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{},"error":{{"code":-32603,"message":"nope"}}}}"#,
        perm.id.unwrap()
    ))
    .await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "end_turn"
    );
    h.close_input().await;
}

#[tokio::test]
async fn unrelated_messages_during_a_permission_wait_are_skipped() {
    let daemon = ScriptedDaemon::new(vec![approval_event(), completed("complete")], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, true).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    let perm = h
        .recv_until(|m| m.method.as_deref() == Some("session/request_permission"))
        .await;
    // Junk lines before the real response are ignored.
    h.send("not json").await;
    h.send(r#"{"jsonrpc":"2.0","id":999,"result":{"unrelated":true}}"#)
        .await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{},"result":{{"outcome":{{"outcome":"selected","optionId":"allow-always"}}}}}}"#,
        perm.id.unwrap()
    ))
    .await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "end_turn"
    );
    h.close_input().await;
}

#[tokio::test]
async fn a_cancel_during_a_permission_wait_cancels_the_run() {
    // No terminal event scripted: the turn only ends because cancel resolves the
    // permission (Answered) and then EOF closes stdin.
    let daemon = ScriptedDaemon::new(vec![approval_event()], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, true).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    let _ = h
        .recv_until(|m| m.method.as_deref() == Some("session/request_permission"))
        .await;
    // Cancel arrives instead of an approval.
    h.send(r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s"}}"#)
        .await;
    // The turn resumes waiting for events; closing stdin ends it.
    h.close_input().await;
    let result = h.recv_until(is_result).await;
    assert_eq!(result.result.unwrap()["stopReason"], "end_turn");
}

#[tokio::test]
async fn eof_during_a_permission_wait_parks_the_turn() {
    let daemon = ScriptedDaemon::new(vec![approval_event()], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, true).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    let _ = h
        .recv_until(|m| m.method.as_deref() == Some("session/request_permission"))
        .await;
    // Close stdin without answering: the permission wait sees EOF and parks.
    h.close_input().await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "end_turn"
    );
}

// ─── cancellation ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_cancel_notification_between_turns_cancels_the_run() {
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = cancelled.clone();
    let daemon = ScriptedDaemon::new(vec![completed("complete")], move |req| match req {
        ControlRequest::Spawn { .. } => ControlResponse::Spawned {
            run_id: RUN_ID.to_string(),
        },
        ControlRequest::Cancel { .. } => {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            ControlResponse::Ok { ok: true }
        }
        _ => ControlResponse::Ok { ok: true },
    });
    let (mut h, _bp) = opened_session(daemon, false).await;
    // Run one prompt so a run_id exists.
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    let _ = h.recv_until(is_result).await;
    // Cancel between turns.
    h.send(r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s"}}"#)
        .await;
    // Prove the cancel reached the daemon by racing a follow-up request through.
    h.send(r#"{"jsonrpc":"2.0","id":5,"method":"initialize"}"#)
        .await;
    let _ = h.recv_until(is_result).await;
    h.close_input().await;
    assert!(cancelled.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn a_cancel_notification_with_no_active_run_is_a_no_op() {
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    let mut h = Harness::start(daemon, AgentClientArgs::default());
    // No session, no run - cancel is simply ignored.
    h.send(r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s"}}"#)
        .await;
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#)
        .await;
    assert!(h.recv().await.result.is_some());
    h.close_input().await;
}

#[tokio::test]
async fn a_cancel_notification_mid_turn_cancels_the_run() {
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = cancelled.clone();
    // No terminal event: the turn is driven only by our cancel then EOF.
    let daemon = ScriptedDaemon::new(vec![status_event()], move |req| match req {
        ControlRequest::Spawn { .. } => ControlResponse::Spawned {
            run_id: RUN_ID.to_string(),
        },
        ControlRequest::Cancel { .. } => {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            ControlResponse::Ok { ok: true }
        }
        _ => ControlResponse::Ok { ok: true },
    });
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    // A non-cancel line and an unparseable line mid-turn are both ignored.
    h.send(r#"{"jsonrpc":"2.0","method":"initialized"}"#).await;
    h.send("not json at all").await;
    // Then cancel mid-turn.
    h.send(r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"s"}}"#)
        .await;
    h.close_input().await;
    let _ = h.recv_until(is_result).await;
    assert!(cancelled.load(std::sync::atomic::Ordering::SeqCst));
}

// ─── daemon unreachable ──────────────────────────────────────────────────────

#[tokio::test]
async fn an_unreachable_daemon_makes_a_prompt_refuse() {
    // A client pointed at a socket with no daemon: subscribe fails.
    let bad = ControlClient::new(control_id(std::path::Path::new("/no/such/daemon")));
    let (bp, args) = blueprint_args();
    let (to_server, server_in) = tokio::io::duplex(64 * 1024);
    let (server_out, from_server) = tokio::io::duplex(1024 * 1024);
    let runs_dir = tempfile::tempdir().unwrap();
    let runs_path = runs_dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        let _ = serve_over(
            BufReader::new(server_in),
            server_out,
            bad,
            args,
            runs_path,
            HARNESS_DEFAULT_CWD.to_string(),
        )
        .await;
    });
    let mut h = Harness {
        to_server,
        from_server: BufReader::new(from_server),
        runs_dir,
        _daemon: ScriptedDaemon::new(vec![], spawn_ok),
        _server: AbortOnDrop(server),
    };
    // Keep the blueprint alive for the whole test.
    let _bp = bp;
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp"}}"#)
        .await;
    // session/new still succeeds (it doesn't touch the daemon)…
    let _ = h.recv().await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    // …but the prompt can't subscribe, so it refuses.
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "refusal"
    );
    h.close_input().await;
}

// ─── large output framing ────────────────────────────────────────────────────

#[tokio::test]
async fn oversized_output_is_split_across_chunks() {
    let daemon = ScriptedDaemon::new(vec![status_event(), completed("complete")], spawn_ok);
    let (mut h, _bp) = opened_session(daemon, false).await;
    // One stage output larger than a single frame.
    let big = "x".repeat(leviath_agent_client::MAX_FRAME_BYTES + 100);
    h.write_output(0, &big);
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    // Collect every chunk until the result; their concatenation is the output.
    let mut assembled = String::new();
    loop {
        let msg = h.recv().await;
        if let Some("agent_message_chunk") = update_kind(&msg).as_deref() {
            assembled.push_str(
                msg.params.unwrap()["update"]["content"]["text"]
                    .as_str()
                    .unwrap(),
            );
        } else if is_result(&msg) {
            break;
        }
    }
    assert_eq!(assembled, big);
    h.close_input().await;
}

// ─── output polled on the tick ───────────────────────────────────────────────

#[tokio::test]
async fn output_is_flushed_on_the_poll_tick_between_events() {
    // A daemon that answers Spawn but delays the terminal event past one poll
    // interval, so the 250 ms tick - not an event - is what flushes the output.
    let dir = tempfile::tempdir().unwrap();
    let id = control_id(dir.path());
    let mut listener = bind_control_listener(&id).unwrap();
    let accept = tokio::spawn(async move {
        loop {
            let Ok(Some(stream)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let (read_half, mut write_half) = tokio::io::split(stream);
                let mut lines = BufReader::new(read_half).lines();
                let Ok(Some(line)) = lines.next_line().await else {
                    return;
                };
                let req = serde_json::from_str::<ControlRequest>(&line).unwrap();
                match req {
                    ControlRequest::Spawn { .. } => {
                        let mut out = serde_json::to_string(&ControlResponse::Spawned {
                            run_id: RUN_ID.to_string(),
                        })
                        .unwrap();
                        out.push('\n');
                        let _ = write_half.write_all(out.as_bytes()).await;
                    }
                    ControlRequest::Subscribe => {
                        // No event for a while, so a poll tick fires first.
                        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                        let mut out = serde_json::to_string(&completed("complete")).unwrap();
                        out.push('\n');
                        let _ = write_half.write_all(out.as_bytes()).await;
                        std::future::pending::<()>().await;
                    }
                    _ => {}
                }
            });
        }
    });
    let daemon = ScriptedDaemon {
        client: ControlClient::new(control_id(dir.path())),
        _dir: dir,
        _accept: AbortOnDrop(accept),
    };
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.write_output(0, "tick-flushed output\n");
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    // The chunk arrives from the poll tick, before the delayed completion.
    let chunk = h
        .recv_until(|m| update_kind(m).as_deref() == Some("agent_message_chunk"))
        .await;
    assert_eq!(
        chunk.params.unwrap()["update"]["content"]["text"],
        "tick-flushed output\n"
    );
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "end_turn"
    );
    h.close_input().await;
}

// ─── stream closed by the daemon ─────────────────────────────────────────────

#[tokio::test]
async fn a_closed_event_stream_ends_the_turn() {
    // The daemon streams nothing and closes the subscribe connection at once, so
    // the client's event stream yields None with no terminal event.
    let dir = tempfile::tempdir().unwrap();
    let id = control_id(dir.path());
    let mut listener = bind_control_listener(&id).unwrap();
    let accept = tokio::spawn(async move {
        loop {
            let Ok(Some(stream)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let (read_half, mut write_half) = tokio::io::split(stream);
                let mut lines = BufReader::new(read_half).lines();
                let Ok(Some(line)) = lines.next_line().await else {
                    return;
                };
                let req = serde_json::from_str::<ControlRequest>(&line).unwrap();
                if let ControlRequest::Spawn { .. } = req {
                    let mut out = serde_json::to_string(&ControlResponse::Spawned {
                        run_id: RUN_ID.to_string(),
                    })
                    .unwrap();
                    out.push('\n');
                    let _ = write_half.write_all(out.as_bytes()).await;
                }
                // For Subscribe (and anything else), drop the connection → EOF.
            });
        }
    });
    let daemon = ScriptedDaemon {
        client: ControlClient::new(control_id(dir.path())),
        _dir: dir,
        _accept: AbortOnDrop(accept),
    };
    let (mut h, _bp) = opened_session(daemon, false).await;
    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"prompt":[{"type":"text","text":"go"}]}}"#)
        .await;
    assert_eq!(
        h.recv_until(is_result).await.result.unwrap()["stopReason"],
        "end_turn"
    );
    h.close_input().await;
}

// ─── pure helpers: run-status → stop reason ──────────────────────────────────

mod run_status_helpers {
    use super::*;
    use leviath_core::run_meta::RunStatus;

    #[test]
    fn read_run_status_reads_the_persisted_status_ignoring_extra_fields() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("r1");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(
            run.join("meta.json"),
            r#"{"status":"complete_interactive","run_id":"r1","extra":1}"#,
        )
        .unwrap();
        assert_eq!(
            read_run_status(dir.path(), "r1"),
            Some(RunStatus::CompleteInteractive)
        );
    }

    #[test]
    fn read_run_status_is_none_when_missing_or_malformed() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file.
        assert_eq!(read_run_status(dir.path(), "nope"), None);
        // Present but not valid JSON.
        let run = dir.path().join("bad");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("meta.json"), "not json").unwrap();
        assert_eq!(read_run_status(dir.path(), "bad"), None);
    }
}
