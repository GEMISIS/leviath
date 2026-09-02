//! End-to-end tests for `lev mcp serve`, driven over an in-memory duplex
//! against the scripted fake daemon the wait loop's tests use.
//!
//! Each test starts [`serve_over_with`] on a background task wired to two
//! `tokio::io::duplex` pipes (the server's stdin and stdout), sends JSON-RPC
//! lines, and reads what comes back. The environment is a temp directory
//! standing in for the machine: a runs dir, an agents dir, a tools dir, and a
//! project directory the runs would work in.

use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};

use leviath_core::run_meta::RunStatus;
use leviath_runtime::control_socket::{ControlRequest, ControlResponse};
use leviath_runtime::host::WorldEvent;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

use super::super::serve_tools::{
    build_interaction_response, cap_text, has_granted_read_paths, missing_bundled,
    timeout_from_secs, wire_status,
};
use crate::daemon::wait::tests::{
    RUN_ID, ScriptedDaemon, StreamScript, completed, completed_with_answer, fast, interaction_for,
    meta_for, no_daemon_client, spawn_ok, stage_transition, status_event, tool_finished_for,
    write_meta, write_run,
};

/// The client protocol revision the tests offer.
const CLIENT_VERSION: &str = "2025-06-18";

/// A `DaemonReady` that always says yes.
fn always_ready() -> DaemonReady {
    Arc::new(|| Box::pin(async { Ok(()) }))
}

/// A `DaemonReady` that fails the first `failures` times, then says yes, and
/// counts how often it was asked.
fn flaky_ready(failures: usize) -> (DaemonReady, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let ready: DaemonReady = Arc::new(move || {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            match n < failures {
                true => Err(format!("attempt {} did not start the daemon", n + 1)),
                false => Ok(()),
            }
        })
    });
    (ready, calls)
}

/// The machine a test server runs on.
struct Machine {
    /// Held for its `Drop`, which removes the directory; every path is read
    /// from `path`.
    _root: tempfile::TempDir,
    /// The root's canonical path, which every derived path is built from.
    ///
    /// `lev run` canonicalises the manifest it resolves, and the stale-install
    /// note fires only when that path `starts_with` the agents directory the
    /// environment names. On Linux a temp dir is already canonical, but macOS
    /// keeps its temp dirs behind a symlink (`/var` is `/private/var`) and
    /// Windows canonicalises to a verbatim `\\?\` path, so a home spelled
    /// the way `tempdir()` gave it never matches there.
    path: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(root.path()).unwrap();
        std::fs::create_dir_all(path.join("project")).unwrap();
        std::fs::create_dir_all(path.join("runs")).unwrap();
        Self { _root: root, path }
    }
    fn runs_dir(&self) -> PathBuf {
        self.path.join("runs")
    }
    fn project(&self) -> PathBuf {
        self.path.join("project")
    }
    fn agents_dir(&self) -> PathBuf {
        self.path.join("home").join(".leviath").join("agents")
    }
    fn tools_dir(&self) -> PathBuf {
        self.path.join("home").join(".leviath").join("tools")
    }
    fn home(&self) -> PathBuf {
        self.path.join("home")
    }
    /// Write the inline coder blueprint under `<agents>/<name>/agent.leviath`
    /// and return the manifest path.
    fn install_agent(&self, name: &str, extra: &str) -> PathBuf {
        let dir = self.agents_dir().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("agent.leviath");
        let mut text = crate::test_support::inline_coder_manifest();
        text.push_str(extra);
        std::fs::write(&manifest, text).unwrap();
        manifest
    }
    fn env(&self, daemon_ready: DaemonReady) -> McpServeEnv {
        McpServeEnv {
            runs_dir: self.runs_dir(),
            default_cwd: self.project().to_string_lossy().to_string(),
            tools_dir: Some(self.tools_dir()),
            agents_dir: Some(self.agents_dir()),
            home: Some(self.home()),
            allowed_workdirs: Vec::new(),
            daemon_ready,
        }
    }
}

/// A running server plus the pipes to drive it.
struct Harness {
    to_server: DuplexStream,
    from_server: BufReader<DuplexStream>,
    server: Option<tokio::task::JoinHandle<()>>,
}

impl Harness {
    fn start(
        control: ControlClient,
        args: McpServeArgs,
        env: McpServeEnv,
        timing: ServeTiming,
    ) -> Self {
        let (to_server, server_in) = tokio::io::duplex(64 * 1024);
        let (server_out, from_server) = tokio::io::duplex(4 * 1024 * 1024);
        let server = tokio::spawn(async move {
            let _ = serve_over_with(
                Box::pin(BufReader::new(server_in)),
                Box::pin(server_out),
                control,
                args,
                env,
                timing,
            )
            .await;
        });
        Self {
            to_server,
            from_server: BufReader::new(from_server),
            server: Some(server),
        }
    }

    /// The usual setup: a scripted daemon, a machine, fast clocks.
    fn usual(daemon: &ScriptedDaemon, machine: &Machine) -> Self {
        Self::start(
            daemon.client(),
            McpServeArgs::default(),
            machine.env(always_ready()),
            fast_timing(),
        )
    }

    async fn send(&mut self, line: &str) {
        self.to_server.write_all(line.as_bytes()).await.unwrap();
        self.to_server.write_all(b"\n").await.unwrap();
        self.to_server.flush().await.unwrap();
    }

    async fn send_json(&mut self, msg: Value) {
        self.send(&msg.to_string()).await;
    }

    /// Call `tool` with `arguments` under request id `id`.
    async fn call(&mut self, id: u64, tool: &str, arguments: Value) {
        self.send_json(json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        }))
        .await;
    }

    async fn close_input(&mut self) {
        self.to_server.shutdown().await.unwrap();
    }

    async fn recv(&mut self) -> JsonRpcMessage {
        let mut line = String::new();
        let n = self.from_server.read_line(&mut line).await.unwrap();
        assert_ne!(n, 0, "server closed its output before sending a message");
        serde_json::from_str(line.trim()).unwrap()
    }

    /// Read until the response to `id`, returning the notifications that came
    /// before it and the response itself.
    async fn response(&mut self, id: u64) -> (Vec<JsonRpcMessage>, JsonRpcMessage) {
        let mut notifications = Vec::new();
        loop {
            let msg = self.recv().await;
            if msg.id == Some(json!(id)) {
                return (notifications, msg);
            }
            notifications.push(msg);
        }
    }

    /// The `tools/call` result for `id`: `(is_error, text, structuredContent)`.
    async fn call_result(&mut self, id: u64) -> (bool, String, Value) {
        let (_, msg) = self.response(id).await;
        result_parts(&msg)
    }

    /// Wait for the server task to finish, which it does at EOF.
    async fn finished(&mut self) {
        let server = self.server.take().unwrap();
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("the server ended")
            .unwrap();
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

fn result_parts(msg: &JsonRpcMessage) -> (bool, String, Value) {
    let result = msg
        .result
        .as_ref()
        .unwrap_or_else(|| panic!("a result, not {:?}", msg.error));
    (
        result["isError"].as_bool().unwrap(),
        result["content"][0]["text"].as_str().unwrap().to_string(),
        result
            .get("structuredContent")
            .cloned()
            .unwrap_or(Value::Null),
    )
}

fn fast_timing() -> ServeTiming {
    ServeTiming {
        heartbeat: Duration::from_secs(60),
        wait: fast(),
    }
}

/// The env overrides every `run` test needs: `LEVIATH_HOME` at the machine's
/// home (so an installed agent name resolves there) and the config isolated.
fn isolation(machine: &Machine) -> Vec<(&'static str, Option<std::ffi::OsString>)> {
    let mut vars = crate::config::config_isolation_vars(&machine.home());
    vars.push(("LEVIATH_HOME", Some(machine.home().into_os_string())));
    vars.push((
        "LEVIATH_RUNS_DIR",
        Some(machine.runs_dir().into_os_string()),
    ));
    vars
}

// ─── Protocol ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_handshake_ping_and_tool_list_need_no_daemon() {
    let machine = Machine::new();
    let mut h = Harness::start(
        no_daemon_client(),
        McpServeArgs::default(),
        machine.env(Arc::new(|| {
            Box::pin(async { Err("never asked".to_string()) })
        })),
        fast_timing(),
    );
    h.send_json(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": CLIENT_VERSION, "capabilities": {}, "clientInfo": { "name": "t", "version": "0" } },
    }))
    .await;
    let init = h.recv().await;
    let result = init.result.unwrap();
    assert_eq!(result["protocolVersion"], CLIENT_VERSION);
    assert_eq!(result["serverInfo"]["name"], "leviath");
    assert!(result["instructions"].as_str().unwrap().contains("levaith"));

    h.send_json(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .await;
    h.send("").await; // a blank keep-alive line
    h.send_json(json!({ "jsonrpc": "2.0", "id": "p-1", "method": "ping" }))
        .await;
    let pong = h.recv().await;
    assert_eq!(pong.id, Some(json!("p-1")));
    assert_eq!(pong.result, Some(json!({})));

    // An `initialize` with no params falls back to the preferred revision.
    h.send_json(json!({ "jsonrpc": "2.0", "id": 2, "method": "initialize" }))
        .await;
    let init = h.recv().await;
    assert_eq!(
        init.result.unwrap()["protocolVersion"],
        leviath_mcp::PREFERRED_PROTOCOL_VERSION
    );

    h.send_json(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }))
        .await;
    let listed = h.recv().await.result.unwrap();
    let tools = listed["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "run",
            "wait",
            "status",
            "result",
            "cancel",
            "message",
            "respond",
            "list_runs",
            "list_agents",
            "install_tool",
            "list_tools"
        ]
    );
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let ann = &tool["annotations"];
        let read_only = [
            "status",
            "result",
            "list_runs",
            "list_agents",
            "list_tools",
            "wait",
        ]
        .contains(&name);
        assert_eq!(ann["readOnlyHint"], read_only, "{name}");
        assert_eq!(ann["destructiveHint"], name == "cancel", "{name}");
        assert_eq!(
            ann["openWorldHint"],
            ["run", "install_tool"].contains(&name),
            "{name}"
        );
        assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
    }
    let run = &tools[0];
    assert!(
        run["description"]
            .as_str()
            .unwrap()
            .contains("Use this instead of spawning a subagent when the user says leviath")
    );
    assert!(
        run["description"]
            .as_str()
            .unwrap()
            .contains("the run continues (list_runs, wait, status, cancel)")
    );
    assert!(
        tools[1]["description"]
            .as_str()
            .unwrap()
            .contains("the run continues")
    );
    // `task` is optional: an agent that takes named inputs gets `regions`.
    assert_eq!(run["inputSchema"]["required"], json!([]));
    assert!(
        run["inputSchema"]["properties"]["task"]["description"]
            .as_str()
            .unwrap()
            .contains("Omit it for an agent that takes named inputs")
    );
}

#[tokio::test]
async fn bad_lines_unknown_methods_and_malformed_calls_are_refused_with_the_right_codes() {
    let machine = Machine::new();
    let mut h = Harness::start(
        no_daemon_client(),
        McpServeArgs::default(),
        machine.env(always_ready()),
        fast_timing(),
    );
    h.send("this is not json").await;
    let err = h.recv().await;
    // A `null` id is written, and reads back as an absent one.
    assert_eq!(err.id.unwrap_or(Value::Null), Value::Null);
    assert_eq!(err.error.unwrap().code, -32700);

    h.send_json(json!({ "jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {} }))
        .await;
    let err = h.recv().await.error.unwrap();
    assert_eq!(err.code, -32601);
    assert_eq!(err.message, "Leviath does not implement 'server/discover'");

    // Notifications are never answered, known or not; nor is a stray response.
    h.send_json(json!({ "jsonrpc": "2.0", "method": "notifications/whatever" }))
        .await;
    h.send_json(json!({ "jsonrpc": "2.0", "id": 99, "result": {} }))
        .await;

    h.send_json(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call" }))
        .await;
    let err = h.recv().await.error.unwrap();
    assert_eq!(err.code, -32602);
    assert!(err.message.contains("needs params"), "{}", err.message);

    h.send_json(
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": 7 } }),
    )
    .await;
    assert!(
        h.recv()
            .await
            .error
            .unwrap()
            .message
            .contains("string `name`")
    );

    h.send_json(json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "status", "arguments": [1] } }))
        .await;
    assert!(
        h.recv()
            .await
            .error
            .unwrap()
            .message
            .contains("JSON object")
    );

    h.call(5, "no_such_tool", json!({})).await;
    let (_, msg) = h.response(5).await;
    let err = msg.error.unwrap();
    assert_eq!(err.code, -32602);
    assert!(
        err.message.contains("unknown tool 'no_such_tool'"),
        "{}",
        err.message
    );

    // A relative workdir is a protocol error, not a tool failure.
    h.call(6, "run", json!({ "task": "x", "workdir": "relative/dir" }))
        .await;
    let (_, msg) = h.response(6).await;
    assert!(msg.error.unwrap().message.contains("absolute path"));

    // A missing required argument likewise, as is a wrong type, an unknown
    // key, a negative count and a word outside an enum: the schema says so.
    h.call(7, "wait", json!({})).await;
    let (_, msg) = h.response(7).await;
    let message = msg.error.unwrap().message;
    assert!(
        message.contains("invalid arguments for 'wait'"),
        "{message}"
    );
    assert!(message.contains("run_id"), "{message}");
    h.call(8, "run", json!({ "task": "t", "yolo": "yes" }))
        .await;
    let (_, msg) = h.response(8).await;
    assert!(msg.error.unwrap().message.contains("yolo"));
    h.call(9, "status", json!({ "run_id": "r", "extra": 1 }))
        .await;
    let (_, msg) = h.response(9).await;
    assert!(msg.error.unwrap().message.contains("extra"));
    h.call(
        10,
        "respond",
        json!({ "request_id": "r", "choice_index": -1 }),
    )
    .await;
    let (_, msg) = h.response(10).await;
    assert!(msg.error.unwrap().message.contains("choice_index"));
    h.call(
        11,
        "respond",
        json!({ "request_id": "r", "scope": "forever" }),
    )
    .await;
    let (_, msg) = h.response(11).await;
    assert!(msg.error.unwrap().message.contains("forever"));

    // No `arguments` at all, or a null one, is an empty object.
    h.send_json(json!({
        "jsonrpc": "2.0", "id": 12, "method": "tools/call",
        "params": { "name": "list_tools" },
    }))
    .await;
    let (is_error, _, _) = h.call_result(12).await;
    assert!(!is_error);
    h.send_json(json!({
        "jsonrpc": "2.0", "id": 13, "method": "tools/call",
        "params": { "name": "list_tools", "arguments": null },
    }))
    .await;
    let (is_error, _, _) = h.call_result(13).await;
    assert!(!is_error);

    h.close_input().await;
    h.finished().await;
}

#[tokio::test]
async fn a_host_that_stops_reading_ends_the_server() {
    let machine = Machine::new();
    let (mut to_server, server_in) = tokio::io::duplex(1024);
    let server = tokio::spawn(serve_over(
        BufReader::new(server_in),
        FailingWriter,
        no_daemon_client(),
        McpServeArgs::default(),
        machine.env(always_ready()),
    ));
    to_server
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n")
        .await
        .unwrap();
    to_server.flush().await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .expect("the server noticed the broken output");
    assert!(outcome.unwrap().is_ok());
}

/// An `AsyncWrite` that fails every write.
struct FailingWriter;

impl tokio::io::AsyncWrite for FailingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Err(std::io::Error::other("write failed")))
    }
    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// Every schema in the table compiles, so the validator never skips one and
/// the handlers' infallible reads are safe. Also holds the table to the
/// dispatch: every name listed is a tool `call_tool` knows.
#[tokio::test]
async fn every_tool_schema_compiles_and_every_tool_is_callable() {
    let table = super::super::serve_tools::tool_table();
    for tool in &table {
        jsonschema::validator_for(&tool.input_schema)
            .unwrap_or_else(|e| panic!("{}: {e}", tool.name));
        assert!(
            tool.description.len() < 1000,
            "{}: hosts show descriptions verbatim",
            tool.name
        );
    }
    let machine = Machine::new();
    let mut h = Harness::start(
        no_daemon_client(),
        McpServeArgs::default(),
        machine.env(always_ready()),
        fast_timing(),
    );
    // Each tool called with nothing: the schema refuses the ones with required
    // arguments, the others answer; none is "unknown".
    for (i, tool) in table.iter().enumerate() {
        h.call(i as u64, &tool.name, json!({})).await;
        let (_, msg) = h.response(i as u64).await;
        if let Some(err) = &msg.error {
            assert!(
                err.message.contains("invalid arguments"),
                "{}: {}",
                tool.name,
                err.message
            );
        }
    }
}

#[test]
fn routing_separates_serve_from_the_managing_subcommands() {
    use super::super::{McpArgs, McpRoute};
    let McpRoute::Serve(serve) = McpArgs::serve_for_test().route() else {
        panic!("serve was routed to manage");
    };
    assert_eq!(serve.default_agent, "orchestrator");
    let McpRoute::Manage(_) = McpArgs::list_for_test().route() else {
        panic!("list was routed to serve");
    };
}

#[test]
fn the_defaults_are_what_the_help_promises() {
    let args = McpServeArgs::default();
    assert!(!args.attended);
    assert!(args.allow.is_empty());
    assert_eq!(args.default_agent, "orchestrator");
    assert!(args.workdir.is_none());
    let timing = ServeTiming::default();
    assert_eq!(timing.heartbeat, Duration::from_secs(15));
    assert_eq!(MCP_TEXT_CAP, 48 * 1024);
}

// ─── run ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn run_spawns_waits_reports_progress_and_returns_the_answer() {
    let machine = Machine::new();
    // Not under `coder/`: an installed copy of a bundled agent at another
    // version gets a stale-install warning, which another test covers.
    let manifest = machine.install_agent("mycoder", "");
    temp_env::async_with_vars(isolation(&machine), async {
        let daemon = ScriptedDaemon::new(
            vec![StreamScript::Hold(vec![
                stage_transition(),
                status_event(),
                tool_finished_for(
                    RUN_ID,
                    "install_tool",
                    true,
                    "Installed tool 'cargo_lint' at /t.",
                ),
                completed_with_answer("Shipped it."),
            ])],
            spawn_ok,
        );
        let mut h = Harness::start(
            daemon.client(),
            McpServeArgs {
                allow: vec!["read_file".to_string()],
                ..McpServeArgs::default()
            },
            machine.env(always_ready()),
            fast_timing(),
        );
        h.send_json(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {
                "name": "run",
                "arguments": {
                    "task": "fix the tests",
                    "agent": manifest.to_string_lossy(),
                    "workdir": machine.project().to_string_lossy(),
                    "allow": ["list_dir"],
                    "model": "anthropic/m",
                    "max_depth": 2,
                    "no_seed_commands": true,
                    "output_format": "markdown",
                    "output_schema": { "type": "object", "required": ["summary"] },
                },
                "_meta": { "progressToken": "tok-1" },
            },
        }))
        .await;
        let (notifications, msg) = h.response(1).await;
        let (is_error, text, structured) = result_parts(&msg);
        assert!(!is_error);
        assert!(text.starts_with("Shipped it."), "{text}");
        assert!(
            text.contains("Installed global tools: cargo_lint (inspect with lev tools)"),
            "{text}"
        );
        assert_eq!(structured["run_id"], RUN_ID);
        assert_eq!(structured["status"], "complete");
        assert_eq!(structured["final_output"]["format"], "markdown");
        assert_eq!(structured["final_output"]["host_truncated"], false);
        assert_eq!(structured["tools_installed"], json!(["cargo_lint"]));
        assert_eq!(structured["warnings"], json!([]));
        assert!(structured.get("host_truncated").is_none());

        // Progress: the run id first, then one per stage transition and
        // status, strictly increasing under the client's token.
        let progress: Vec<&JsonRpcMessage> = notifications
            .iter()
            .filter(|n| n.method.as_deref() == Some("notifications/progress"))
            .collect();
        assert_eq!(progress.len(), 3, "{notifications:?}");
        let params: Vec<&Value> = progress
            .iter()
            .map(|n| n.params.as_ref().unwrap())
            .collect();
        assert!(params.iter().all(|p| p["progressToken"] == "tok-1"));
        assert_eq!(params[0]["progress"], 1);
        assert_eq!(params[0]["message"], format!("run {RUN_ID} started"));
        assert_eq!(params[1]["progress"], 2);
        assert!(
            params[1]["message"]
                .as_str()
                .unwrap()
                .contains("entered stage implement")
        );
        assert_eq!(params[2]["progress"], 3);
        assert!(
            params[2]["message"]
                .as_str()
                .unwrap()
                .contains("active in implement")
        );

        // The spawn carried the call's arguments and the server's defaults.
        let requests = daemon.requests();
        let ControlRequest::Spawn { args } = &requests[0] else {
            panic!("{requests:?}");
        };
        assert_eq!(args.task, "fix the tests");
        assert!(args.yolo, "unattended by default");
        assert_eq!(args.allow, vec!["read_file", "list_dir"]);
        assert_eq!(args.model.as_deref(), Some("anthropic/m"));
        assert_eq!(args.max_depth, Some(2));
        assert!(args.no_seed_commands);
        assert_eq!(args.workdir, machine.project().to_string_lossy());
        assert_eq!(
            args.output.as_ref().unwrap().format.as_deref(),
            Some("markdown")
        );
        assert_eq!(
            args.output.as_ref().unwrap().schema.as_ref().unwrap()["required"],
            json!(["summary"])
        );
    })
    .await;
}

#[tokio::test]
async fn run_without_a_token_emits_no_progress_and_a_null_token_counts_as_none() {
    let machine = Machine::new();
    let manifest = machine.install_agent("coder", "");
    temp_env::async_with_vars(isolation(&machine), async {
        let daemon = ScriptedDaemon::new(
            vec![StreamScript::Hold(vec![status_event(), completed("complete")])],
            spawn_ok,
        );
        let mut h = Harness::usual(&daemon, &machine);
        let agent = manifest.to_string_lossy().to_string();
        h.call(1, "run", json!({ "task": "t", "agent": agent, "workdir": machine.project().to_string_lossy() })).await;
        let (notifications, msg) = h.response(1).await;
        assert!(notifications.is_empty(), "{notifications:?}");
        let (is_error, text, structured) = result_parts(&msg);
        assert!(!is_error);
        assert!(text.contains("no final output; see `result`/`status`"), "{text}");
        assert_eq!(structured["final_output"], Value::Null);

        h.send_json(json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "run", "arguments": { "task": "t", "agent": agent, "workdir": machine.project().to_string_lossy() }, "_meta": { "progressToken": null } },
        }))
        .await;
        let (notifications, _) = h.response(2).await;
        assert!(notifications.is_empty(), "{notifications:?}");
    })
    .await;
}

#[tokio::test]
async fn run_with_wait_false_returns_the_run_id_at_once() {
    let machine = Machine::new();
    let manifest = machine.install_agent("coder", "");
    temp_env::async_with_vars(isolation(&machine), async {
        let daemon = ScriptedDaemon::new(vec![StreamScript::Hold(vec![])], spawn_ok);
        let mut h = Harness::start(
            daemon.client(),
            McpServeArgs {
                attended: true,
                workdir: Some(machine.project()),
                ..McpServeArgs::default()
            },
            machine.env(always_ready()),
            fast_timing(),
        );
        h.call(
            1,
            "run",
            json!({ "task": "t", "agent": manifest.to_string_lossy(), "wait": false }),
        )
        .await;
        let (is_error, text, structured) = h.call_result(1).await;
        assert!(!is_error);
        assert!(text.contains(&format!("run {RUN_ID} started")), "{text}");
        assert_eq!(structured["run_id"], RUN_ID);
        assert_eq!(structured["status"], "starting");
        let ControlRequest::Spawn { args } = &daemon.requests()[0] else {
            panic!()
        };
        assert!(!args.yolo, "--attended flips the default");
        assert_eq!(args.workdir, machine.project().to_string_lossy());
    })
    .await;
}

#[tokio::test]
async fn run_stops_waiting_at_its_timeout_and_leaves_the_run_alone() {
    let machine = Machine::new();
    let manifest = machine.install_agent("coder", "");
    temp_env::async_with_vars(isolation(&machine), async {
        write_meta(
            &machine.runs_dir(),
            &meta_for(RUN_ID, RunStatus::Running, None),
        );
        let daemon = ScriptedDaemon::new(vec![StreamScript::Hold(vec![])], spawn_ok);
        let mut h = Harness::usual(&daemon, &machine);
        h.call(
            1,
            "run",
            json!({ "task": "t", "agent": manifest.to_string_lossy(), "timeout_secs": 1 }),
        )
        .await;
        let (is_error, text, structured) = h.call_result(1).await;
        assert!(!is_error);
        assert!(text.contains("was not cancelled"), "{text}");
        assert_eq!(structured["timed_out"], true);
        assert_eq!(structured["status"], "running");
        assert!(
            daemon
                .requests()
                .iter()
                .all(|r| matches!(r, ControlRequest::Spawn { .. }))
        );
    })
    .await;
}

#[tokio::test]
async fn run_reports_a_failed_run_a_question_and_a_lost_daemon() {
    let machine = Machine::new();
    let manifest = machine.install_agent("coder", "");
    temp_env::async_with_vars(isolation(&machine), async {
        let agent = manifest.to_string_lossy().to_string();
        let mut meta = meta_for(RUN_ID, RunStatus::Error, None);
        meta.error = Some("the model gave up".to_string());
        meta.current_stage = "implement".to_string();
        write_meta(&machine.runs_dir(), &meta);
        let daemon =
            ScriptedDaemon::new(vec![StreamScript::Hold(vec![completed("error")])], spawn_ok);
        let mut h = Harness::usual(&daemon, &machine);
        h.call(1, "run", json!({ "task": "t", "agent": agent }))
            .await;
        let (is_error, text, structured) = h.call_result(1).await;
        assert!(is_error);
        assert!(text.contains("error: the model gave up"), "{text}");
        assert_eq!(structured["status"], "error");
        assert_eq!(structured["stage"], "implement");
        assert_eq!(structured["error"], "the model gave up");
        // The agent is an installed copy of a bundled one at another version:
        // the pre-flight note `lev run` prints reaches the host as a warning,
        // in the text and in the data.
        let warning = structured["warnings"][0].as_str().unwrap();
        assert!(
            warning.contains("'coder' is installed at 0.0.0"),
            "{warning}"
        );
        assert!(text.starts_with(warning), "{text}");

        std::fs::remove_dir_all(machine.runs_dir().join(RUN_ID)).unwrap();
        let daemon = ScriptedDaemon::new(
            vec![StreamScript::Hold(vec![interaction_for(RUN_ID, "q-7")])],
            spawn_ok,
        );
        let mut h = Harness::usual(&daemon, &machine);
        h.call(2, "run", json!({ "task": "t", "agent": agent }))
            .await;
        let (is_error, text, structured) = h.call_result(2).await;
        assert!(!is_error);
        assert!(text.contains("call respond with request_id=q-7"), "{text}");
        assert_eq!(structured["status"], "waiting_input");
        assert_eq!(structured["request_id"], "q-7");
        assert_eq!(structured["interaction"]["id"], "q-7");

        let daemon = ScriptedDaemon::new(vec![StreamScript::Drop(vec![])], spawn_ok);
        let mut h = Harness::usual(&daemon, &machine);
        h.call(3, "run", json!({ "task": "t", "agent": agent }))
            .await;
        let (is_error, text, structured) = h.call_result(3).await;
        assert!(is_error);
        assert!(text.contains("lost track of run"), "{text}");
        assert_eq!(structured["lost"], true);
    })
    .await;
}

#[tokio::test]
async fn run_refuses_bad_workdirs_before_touching_anything() {
    let machine = Machine::new();
    let daemon = ScriptedDaemon::new(vec![StreamScript::Hold(vec![])], spawn_ok);
    let home = machine.path.clone();
    let mut env = machine.env(always_ready());
    env.home = Some(home.clone());
    let mut h = Harness::start(daemon.client(), McpServeArgs::default(), env, fast_timing());

    let missing = machine.path.join("nowhere");
    h.call(
        1,
        "run",
        json!({ "task": "t", "workdir": missing.to_string_lossy() }),
    )
    .await;
    let (is_error, text, _) = h.call_result(1).await;
    assert!(is_error);
    assert!(text.contains("not a usable directory"), "{text}");

    let file = machine.path.join("a-file");
    std::fs::write(&file, "x").unwrap();
    h.call(
        2,
        "run",
        json!({ "task": "t", "workdir": file.to_string_lossy() }),
    )
    .await;
    let (is_error, text, _) = h.call_result(2).await;
    assert!(is_error);
    assert!(text.contains("not a usable directory"), "{text}");

    h.call(
        3,
        "run",
        json!({ "task": "t", "workdir": home.to_string_lossy() }),
    )
    .await;
    let (is_error, text, structured) = h.call_result(3).await;
    assert!(is_error);
    assert!(
        text.contains("is your home directory (or a filesystem root)"),
        "{text}"
    );
    assert!(text.contains("allowed_workdirs"), "{text}");
    assert_eq!(structured["refused"], "workdir");
    assert!(daemon.requests().is_empty());
}

/// A home reached through a symlink (Fedora Silverblue's `/home` is one) is
/// refused like any other: the guard compares the canonical workdir against
/// the canonical home, not against the spelling `dirs::home_dir()` answered.
#[cfg(unix)]
#[tokio::test]
async fn run_refuses_a_symlinked_home_given_as_its_real_path() {
    let machine = Machine::new();
    let daemon = ScriptedDaemon::new(vec![StreamScript::Hold(vec![])], spawn_ok);
    let real = machine.path.join("real-home");
    std::fs::create_dir_all(&real).unwrap();
    let link = machine.path.join("home-link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let mut env = machine.env(always_ready());
    env.home = Some(link);
    let mut h = Harness::start(daemon.client(), McpServeArgs::default(), env, fast_timing());
    h.call(
        1,
        "run",
        json!({ "task": "t", "workdir": real.to_string_lossy() }),
    )
    .await;
    let (is_error, text, structured) = h.call_result(1).await;
    assert!(is_error);
    assert!(text.contains("is your home directory"), "{text}");
    assert_eq!(structured["refused"], "workdir");
    assert!(daemon.requests().is_empty());
}

#[tokio::test]
async fn run_reports_an_agent_it_cannot_resolve_and_a_daemon_it_cannot_reach() {
    let machine = Machine::new();
    let manifest = machine.install_agent("coder", "");
    temp_env::async_with_vars(isolation(&machine), async {
        let agent = manifest.to_string_lossy().to_string();
        // Unresolvable agent: the default one is not installed here.
        let daemon = ScriptedDaemon::new(vec![StreamScript::Hold(vec![])], spawn_ok);
        let mut h = Harness::start(
            daemon.client(),
            McpServeArgs {
                default_agent: "no-such-agent".to_string(),
                ..McpServeArgs::default()
            },
            machine.env(always_ready()),
            fast_timing(),
        );
        h.call(1, "run", json!({ "task": "t" })).await;
        let (is_error, text, _) = h.call_result(1).await;
        assert!(is_error);
        assert!(
            text.contains("could not start agent 'no-such-agent'"),
            "{text}"
        );

        // A task-less region agent given a task is refused the same way.
        h.call(
            2,
            "run",
            json!({ "task": "t", "agent": agent, "regions": { "nope": "x" } }),
        )
        .await;
        let (is_error, text, _) = h.call_result(2).await;
        assert!(is_error);
        assert!(text.contains("unknown region"), "{text}");

        // The daemon will not start: the factory's words come back, and it is
        // asked again next time - only success is remembered.
        let (ready, calls) = flaky_ready(1);
        let mut env = machine.env(ready);
        env.agents_dir = None;
        let mut h = Harness::start(daemon.client(), McpServeArgs::default(), env, fast_timing());
        h.call(
            3,
            "run",
            json!({ "task": "t", "agent": agent, "wait": false }),
        )
        .await;
        let (is_error, text, _) = h.call_result(3).await;
        assert!(is_error);
        assert!(
            text.contains("not available: attempt 1 did not start"),
            "{text}"
        );
        h.call(
            4,
            "run",
            json!({ "task": "t", "agent": agent, "wait": false }),
        )
        .await;
        let (is_error, _, structured) = h.call_result(4).await;
        assert!(!is_error);
        assert_eq!(structured["status"], "starting");
        h.call(
            5,
            "run",
            json!({ "task": "t", "agent": agent, "wait": false }),
        )
        .await;
        let (is_error, _, _) = h.call_result(5).await;
        assert!(!is_error);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "success was not cached");

        // Ready, but nothing answers the socket.
        let mut h = Harness::start(
            no_daemon_client(),
            McpServeArgs::default(),
            machine.env(always_ready()),
            fast_timing(),
        );
        h.call(6, "run", json!({ "task": "t", "agent": agent }))
            .await;
        let (is_error, text, _) = h.call_result(6).await;
        assert!(is_error);
        assert!(text.contains("not reachable"), "{text}");

        // The daemon refuses the spawn.
        let refusing = ScriptedDaemon::new(vec![StreamScript::Hold(vec![])], |_| {
            ControlResponse::Error {
                message: "blueprint has no stages".to_string(),
            }
        });
        let mut h = Harness::usual(&refusing, &machine);
        h.call(7, "run", json!({ "task": "t", "agent": agent }))
            .await;
        let (is_error, text, _) = h.call_result(7).await;
        assert!(is_error);
        assert!(
            text.contains("spawn failed: blueprint has no stages"),
            "{text}"
        );
    })
    .await;
}

#[tokio::test]
async fn run_installs_a_missing_bundled_agent_first_and_says_so() {
    let machine = Machine::new();
    temp_env::async_with_vars(isolation(&machine), async {
        let daemon = ScriptedDaemon::new(vec![StreamScript::Hold(vec![])], spawn_ok);
        let mut h = Harness::usual(&daemon, &machine);
        assert!(!machine.agents_dir().join("coder").exists());
        h.call(
            1,
            "run",
            json!({ "task": "t", "agent": "coder", "wait": false }),
        )
        .await;
        let (is_error, text, structured) = h.call_result(1).await;
        assert!(!is_error, "{text}");
        assert!(text.contains("installed bundled agent 'coder'"), "{text}");
        assert_eq!(structured["installed_agents"].as_array().unwrap().len(), 1);
        assert!(
            machine
                .agents_dir()
                .join("coder")
                .join("agent.leviath")
                .exists()
        );

        // Already installed now: nothing to say.
        h.call(
            2,
            "run",
            json!({ "task": "t", "agent": "coder", "wait": false }),
        )
        .await;
        let (_, text, structured) = h.call_result(2).await;
        assert!(!text.contains("installed bundled"), "{text}");
        assert_eq!(structured["installed_agents"], json!([]));

        // An agents dir that is a file: the install fails and says how to fix it.
        let file = machine.path.join("agents-file");
        std::fs::write(&file, "x").unwrap();
        let mut env = machine.env(always_ready());
        env.agents_dir = Some(file);
        let mut h = Harness::start(daemon.client(), McpServeArgs::default(), env, fast_timing());
        h.call(
            3,
            "run",
            json!({ "task": "t", "agent": "coder", "wait": false }),
        )
        .await;
        let (is_error, text, _) = h.call_result(3).await;
        assert!(is_error);
        assert!(text.contains("could not be installed"), "{text}");
        assert!(text.contains("lev integrate"), "{text}");
    })
    .await;
}

#[tokio::test]
async fn run_requires_an_explicit_yolo_for_an_agent_whose_read_paths_are_granted() {
    let machine = Machine::new();
    let manifest = machine.install_agent("reader", "\n[read_paths]\nallow = [\"~/notes\"]\n");
    temp_env::async_with_vars(isolation(&machine), async {
        std::fs::write(
            machine.home().join("config.toml"),
            "[security]\nallow_blueprint_read_paths = true\n",
        )
        .unwrap();
        let daemon = ScriptedDaemon::new(vec![StreamScript::Hold(vec![])], spawn_ok);
        let mut h = Harness::usual(&daemon, &machine);
        let agent = manifest.to_string_lossy().to_string();
        h.call(1, "run", json!({ "task": "t", "agent": agent }))
            .await;
        let (is_error, text, structured) = h.call_result(1).await;
        assert!(is_error);
        assert!(text.contains("pass `yolo` explicitly"), "{text}");
        assert_eq!(structured["refused"], "yolo");
        assert!(daemon.requests().is_empty());

        h.call(
            2,
            "run",
            json!({ "task": "t", "agent": agent, "yolo": true, "wait": false }),
        )
        .await;
        let (is_error, _, structured) = h.call_result(2).await;
        assert!(!is_error);
        assert_eq!(structured["status"], "starting");
    })
    .await;
}

#[test]
fn granted_read_paths_are_false_when_the_blueprint_or_config_cannot_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let mut spawn = leviath_runtime::host::SpawnArgs {
        blueprint_path: dir.path().join("missing").to_string_lossy().to_string(),
        workdir: dir.path().to_string_lossy().to_string(),
        ..Default::default()
    };
    assert!(!has_granted_read_paths(&spawn));
    let broken = dir.path().join("agent.leviath");
    std::fs::write(&broken, "not = [toml").unwrap();
    spawn.blueprint_path = broken.to_string_lossy().to_string();
    assert!(!has_granted_read_paths(&spawn));
    let manifest = dir.path().join("ok.leviath");
    std::fs::write(
        &manifest,
        format!(
            "{}\n[read_paths]\nallow = [\"~/notes\"]\n",
            crate::test_support::inline_coder_manifest()
        ),
    )
    .unwrap();
    spawn.blueprint_path = manifest.to_string_lossy().to_string();
    crate::config::with_isolated_config_path("mcp_granted_read_paths", |fake| {
        std::fs::write(fake.join("config.toml"), "not = [toml").unwrap();
        assert!(!has_granted_read_paths(&spawn));
        std::fs::write(
            fake.join("config.toml"),
            "[security]\nallow_blueprint_read_paths = true\n",
        )
        .unwrap();
        assert!(has_granted_read_paths(&spawn));
    });
}

#[test]
fn the_bundled_agents_a_run_installs_are_the_missing_ones_it_needs() {
    let dir = tempfile::tempdir().unwrap();
    assert!(missing_bundled("coder", None).is_empty());
    assert!(missing_bundled("not-bundled", Some(dir.path())).is_empty());
    let coder: Vec<&str> = missing_bundled("coder", Some(dir.path()))
        .iter()
        .map(|b| b.name)
        .collect();
    assert_eq!(coder, ["coder"]);
    // The orchestrator's workers are coders, so it brings coder along.
    let orchestrator: Vec<&str> = missing_bundled("orchestrator", Some(dir.path()))
        .iter()
        .map(|b| b.name)
        .collect();
    assert!(orchestrator.contains(&"coder"), "{orchestrator:?}");
    // The researchers fan out to `researcher`, which names itself as its own
    // worker: read off the manifests, and no loop.
    let names = |agent: &str| -> Vec<&str> {
        missing_bundled(agent, Some(dir.path()))
            .iter()
            .map(|b| b.name)
            .collect()
    };
    let deep = names("deep-researcher");
    assert!(deep.contains(&"researcher"), "{deep:?}");
    assert!(deep.contains(&"deep-researcher"), "{deep:?}");
    assert_eq!(deep.len(), 2, "{deep:?}");
    assert!(names("wide-researcher").contains(&"researcher"));
    assert_eq!(names("researcher"), ["researcher"]);
    // A fan-out to one of the agent's own stages brings nothing along.
    assert_eq!(names("reviewer"), ["reviewer"]);
    std::fs::create_dir_all(dir.path().join("coder")).unwrap();
    std::fs::write(dir.path().join("coder").join("agent.leviath"), "").unwrap();
    assert!(missing_bundled("coder", Some(dir.path())).is_empty());
    // An installed worker is not installed again, but the agent itself is.
    assert_eq!(names("orchestrator"), ["orchestrator"]);
}

#[test]
fn the_wait_deadline_is_none_for_zero_and_clamped_for_a_huge_timeout() {
    assert_eq!(timeout_from_secs(0), None);
    assert_eq!(timeout_from_secs(5), Some(Duration::from_secs(5)));
    assert_eq!(
        timeout_from_secs(u64::MAX),
        Some(Duration::from_secs(u64::from(u32::MAX)))
    );
}

#[tokio::test]
async fn run_takes_named_inputs_without_a_task_and_explains_a_task_an_agent_cannot_take() {
    let machine = Machine::new();
    let coder = machine.install_agent("coder", "");
    // A blueprint with neither a task region nor any caller input.
    let notask_dir = machine.agents_dir().join("notask");
    std::fs::create_dir_all(&notask_dir).unwrap();
    std::fs::write(
        notask_dir.join("agent.leviath"),
        crate::test_support::inline_coder_manifest()
            .replace("task = { kind = \"pinned\", max_tokens = 2000 }\n", ""),
    )
    .unwrap();
    temp_env::async_with_vars(isolation(&machine), async {
        let daemon = ScriptedDaemon::new(vec![StreamScript::Hold(vec![])], spawn_ok);
        let mut h = Harness::usual(&daemon, &machine);

        // The bundled reviewer takes a diff, not a task: the refusal names
        // the `regions` argument and the inputs it takes.
        h.call(
            1,
            "run",
            json!({ "task": "review this", "agent": "reviewer", "wait": false }),
        )
        .await;
        let (is_error, text, structured) = h.call_result(1).await;
        assert!(is_error);
        assert!(
            text.contains("reviewer takes no task; pass regions: {\"diff\": ...}"),
            "{text}"
        );
        assert!(
            text.contains("its caller inputs are diff, criteria"),
            "{text}"
        );
        assert_eq!(structured["refused"], "task");

        // Named inputs and no task: the run starts.
        h.call(
            2,
            "run",
            json!({ "agent": "reviewer", "regions": { "diff": "--- a\n+++ b" }, "wait": false }),
        )
        .await;
        let (is_error, text, structured) = h.call_result(2).await;
        assert!(!is_error, "{text}");
        assert_eq!(structured["status"], "starting");

        // An agent that wants a task and gets none: the resolver's own words,
        // with no editor opened.
        h.call(
            3,
            "run",
            json!({ "agent": coder.to_string_lossy(), "wait": false }),
        )
        .await;
        let (is_error, text, structured) = h.call_result(3).await;
        assert!(is_error);
        assert!(text.contains("could not start agent"), "{text}");
        assert!(text.contains("No task provided"), "{text}");
        assert!(structured.get("refused").is_none());

        // No task region and no caller inputs either.
        h.call(
            4,
            "run",
            json!({ "task": "t", "agent": "notask", "wait": false }),
        )
        .await;
        let (is_error, text, _) = h.call_result(4).await;
        assert!(is_error);
        assert!(
            text.contains("coder takes no task and no caller inputs; leave `task` out"),
            "{text}"
        );

        // A different refusal for a task-less agent is passed through as is.
        h.call(
            5,
            "run",
            json!({ "task": "t", "agent": "notask", "regions": { "nope": "x" }, "wait": false }),
        )
        .await;
        let (is_error, text, structured) = h.call_result(5).await;
        assert!(is_error);
        assert!(text.contains("unknown region"), "{text}");
        assert!(structured.get("refused").is_none());
    })
    .await;
}

#[tokio::test]
async fn a_duplicate_request_id_is_refused_and_the_first_call_left_alone() {
    let machine = Machine::new();
    let manifest = machine.install_agent("coder", "");
    temp_env::async_with_vars(isolation(&machine), async {
        let daemon = ScriptedDaemon::new(vec![StreamScript::Hold(vec![status_event()])], spawn_ok);
        let mut h = Harness::usual(&daemon, &machine);
        h.send_json(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "run", "arguments": { "task": "t", "agent": manifest.to_string_lossy() }, "_meta": { "progressToken": "t" } },
        }))
        .await;
        let started = h.recv().await;
        assert_eq!(started.method.as_deref(), Some("notifications/progress"));

        // The same id again, while the run is still being waited on.
        h.call(1, "list_tools", json!({})).await;
        let refused = loop {
            let frame = h.recv().await;
            if frame.id.is_some() {
                break frame;
            }
        };
        assert_eq!(refused.id, Some(json!(1)));
        let error = refused.error.expect("an error, not a result");
        assert_eq!(error.code, error_codes::INVALID_REQUEST);
        assert!(
            error.message.contains("duplicate request id"),
            "{}",
            error.message
        );

        // The server still answers, and nothing was spawned twice.
        h.send_json(json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }))
            .await;
        let next = loop {
            let frame = h.recv().await;
            if frame.id.is_some() {
                break frame;
            }
        };
        assert_eq!(next.id, Some(json!(2)), "{next:?}");
        assert_eq!(
            daemon
                .requests()
                .iter()
                .filter(|r| matches!(r, ControlRequest::Spawn { .. }))
                .count(),
            1
        );
        // The first call is still in flight when the host goes away, and the
        // server still ends.
        h.close_input().await;
        h.finished().await;
    })
    .await;
}

#[tokio::test]
async fn the_daemon_is_started_again_after_it_went_away() {
    let machine = Machine::new();
    let daemon = ScriptedDaemon::new(vec![], spawn_ok);
    let (ready, calls) = flaky_ready(0);
    let mut h = Harness::start(
        daemon.client(),
        McpServeArgs::default(),
        machine.env(ready),
        fast_timing(),
    );
    h.call(1, "message", json!({ "run_id": RUN_ID, "content": "hi" }))
        .await;
    let (is_error, _, _) = h.call_result(1).await;
    assert!(!is_error);
    h.call(2, "message", json!({ "run_id": RUN_ID, "content": "hi" }))
        .await;
    let (is_error, _, _) = h.call_result(2).await;
    assert!(!is_error);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "success is remembered");

    // `lev daemon stop`: the next call finds nobody, and the one after that
    // asks the factory again instead of failing for the rest of the session.
    daemon.shutdown();
    h.call(3, "message", json!({ "run_id": RUN_ID, "content": "hi" }))
        .await;
    let (is_error, text, _) = h.call_result(3).await;
    assert!(is_error);
    assert!(text.contains("not reachable"), "{text}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    h.call(4, "message", json!({ "run_id": RUN_ID, "content": "hi" }))
        .await;
    let (is_error, text, _) = h.call_result(4).await;
    assert!(is_error);
    assert!(text.contains("not reachable"), "{text}");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the daemon was started again"
    );
}

#[tokio::test]
async fn a_waiting_call_heartbeats_under_the_clients_token() {
    let machine = Machine::new();
    let manifest = machine.install_agent("mycoder", "");
    temp_env::async_with_vars(isolation(&machine), async {
        let daemon = ScriptedDaemon::new(vec![StreamScript::Hold(vec![])], spawn_ok);
        let mut h = Harness::start(
            daemon.client(),
            McpServeArgs::default(),
            machine.env(always_ready()),
            ServeTiming {
                heartbeat: Duration::from_millis(30),
                wait: fast(),
            },
        );
        let runs = machine.runs_dir();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            write_run(&runs, RUN_ID, RunStatus::Complete, Some("Eventually."));
        });
        h.send_json(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "run", "arguments": { "task": "t", "agent": manifest.to_string_lossy() }, "_meta": { "progressToken": 42 } },
        }))
        .await;
        let (notifications, msg) = h.response(1).await;
        let (_, text, _) = result_parts(&msg);
        assert_eq!(text, "Eventually.");
        let heartbeats = notifications
            .iter()
            .filter(|n| n.params.as_ref().unwrap()["message"].as_str().unwrap().contains("still running"))
            .count();
        assert!(heartbeats >= 1, "{notifications:?}");
        let progress: Vec<u64> = notifications
            .iter()
            .map(|n| n.params.as_ref().unwrap()["progress"].as_u64().unwrap())
            .collect();
        assert!(progress.windows(2).all(|w| w[0] < w[1]), "{progress:?}");
        assert!(notifications.iter().all(|n| n.params.as_ref().unwrap()["progressToken"] == 42));
        writer.await.unwrap();
    })
    .await;
}

// ─── notifications/cancelled and shutdown ─────────────────────────────────────

#[tokio::test]
async fn a_host_cancellation_stops_the_waiting_and_not_the_run() {
    let machine = Machine::new();
    let manifest = machine.install_agent("coder", "");
    temp_env::async_with_vars(isolation(&machine), async {
        let _tracing = leviath_testkit::tracing_guard();
        let daemon = ScriptedDaemon::new(vec![StreamScript::Hold(vec![status_event()])], spawn_ok);
        let mut h = Harness::usual(&daemon, &machine);
        h.send_json(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "run", "arguments": { "task": "t", "agent": manifest.to_string_lossy() }, "_meta": { "progressToken": "t" } },
        }))
        .await;
        // The run started (its first progress line names it) before the host
        // gives up on the call.
        let started = h.recv().await;
        assert_eq!(started.method.as_deref(), Some("notifications/progress"));
        h.send_json(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 1, "reason": "Request timed out" } }))
            .await;
        // Unknown ids and missing params are ignored.
        h.send_json(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 77 } }))
            .await;
        h.send_json(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled" }))
            .await;
        // The server is still answering, and the call itself never answers.
        // A progress notification for the scripted status event may still be
        // in flight when the ping is answered, so read past notifications to
        // the next response rather than asserting on frame order.
        h.send_json(json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }))
            .await;
        let next = loop {
            let frame = h.recv().await;
            if frame.id.is_some() {
                break frame;
            }
        };
        assert_eq!(next.id, Some(json!(2)), "{next:?}");
        assert!(daemon.requests().iter().all(|r| matches!(r, ControlRequest::Spawn { .. })), "the run was cancelled");

        // A second call is cancelled before it has a run: the log line copes.
        let mut env = machine.env(Arc::new(|| Box::pin(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        })));
        env.agents_dir = None;
        let mut h = Harness::start(daemon.client(), McpServeArgs::default(), env, fast_timing());
        h.call(3, "run", json!({ "task": "t", "agent": manifest.to_string_lossy() })).await;
        h.send_json(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 3 } }))
            .await;
        h.send_json(json!({ "jsonrpc": "2.0", "id": 4, "method": "ping" }))
            .await;
        assert_eq!(h.recv().await.id, Some(json!(4)));
        // Closing stdin while a call is in flight still ends the server.
        h.close_input().await;
        h.finished().await;
    })
    .await;
}

// ─── wait / status / result ───────────────────────────────────────────────────

#[tokio::test]
async fn wait_follows_an_existing_run_and_refuses_one_it_does_not_know() {
    let machine = Machine::new();
    let daemon = ScriptedDaemon::new(
        vec![StreamScript::Hold(vec![completed_with_answer(
            "Waited for.",
        )])],
        spawn_ok,
    );
    let mut h = Harness::usual(&daemon, &machine);
    h.call(1, "wait", json!({ "run_id": RUN_ID })).await;
    let (is_error, text, _) = h.call_result(1).await;
    assert!(is_error);
    assert!(text.contains("no run"), "{text}");

    write_meta(
        &machine.runs_dir(),
        &meta_for(RUN_ID, RunStatus::Running, None),
    );
    h.call(2, "wait", json!({ "run_id": RUN_ID, "timeout_secs": 30 }))
        .await;
    let (is_error, text, structured) = h.call_result(2).await;
    assert!(!is_error);
    assert_eq!(text, "Waited for.");
    assert_eq!(structured["status"], "complete");

    // A long answer is cut at the host cap and says where the rest is; the
    // largest timeout the schema admits is no problem for the clock.
    write_run(
        &machine.runs_dir(),
        "long",
        RunStatus::Complete,
        Some(&"w".repeat(MCP_TEXT_CAP + 1)),
    );
    h.call(
        5,
        "wait",
        json!({ "run_id": "long", "timeout_secs": u64::MAX }),
    )
    .await;
    let (is_error, text, structured) = h.call_result(5).await;
    assert!(!is_error);
    let (body, note) = text.rsplit_once('\n').unwrap();
    assert_eq!(body.len(), MCP_TEXT_CAP);
    assert!(
        note.starts_with("output truncated for the host; full text with `result`"),
        "{note}"
    );
    assert!(note.contains("final_output"), "{note}");
    assert_eq!(structured["host_truncated"], true);
    assert_eq!(structured["final_output"]["host_truncated"], true);

    let (ready, _) = flaky_ready(usize::MAX);
    let mut h = Harness::start(
        daemon.client(),
        McpServeArgs::default(),
        machine.env(ready),
        fast_timing(),
    );
    h.call(3, "wait", json!({ "run_id": RUN_ID })).await;
    let (is_error, text, _) = h.call_result(3).await;
    assert!(is_error);
    assert!(text.contains("not available"), "{text}");

    let mut h = Harness::start(
        no_daemon_client(),
        McpServeArgs::default(),
        machine.env(always_ready()),
        fast_timing(),
    );
    h.call(4, "wait", json!({ "run_id": RUN_ID })).await;
    let (is_error, text, _) = h.call_result(4).await;
    assert!(is_error);
    assert!(text.contains("not reachable"), "{text}");
}

#[tokio::test]
async fn status_reads_the_record_and_asks_the_daemon_only_about_a_live_run() {
    let machine = Machine::new();
    let daemon = ScriptedDaemon::new(vec![], |req| match req {
        ControlRequest::Status { run_id } if run_id == RUN_ID => ControlResponse::Status {
            status: Some(leviath_runtime::components::AgentStatus::Waiting),
        },
        _ => ControlResponse::Status { status: None },
    });
    let mut h = Harness::usual(&daemon, &machine);
    h.call(1, "status", json!({ "run_id": "nope" })).await;
    let (is_error, text, _) = h.call_result(1).await;
    assert!(is_error);
    assert!(text.contains("no run 'nope'"), "{text}");

    let mut done = meta_for("done-run", RunStatus::Complete, None);
    done.error = Some("nothing, really".to_string());
    write_run(
        &machine.runs_dir(),
        "done-run",
        RunStatus::Complete,
        Some("out"),
    );
    h.call(2, "status", json!({ "run_id": "done-run" })).await;
    let (is_error, text, structured) = h.call_result(2).await;
    assert!(!is_error);
    assert!(text.contains("final output available"), "{text}");
    assert_eq!(structured["status"], "complete");
    assert_eq!(structured["live"], false);
    assert_eq!(structured["has_final_output"], true);
    assert!(
        daemon.requests().is_empty(),
        "a finished run asked the daemon"
    );

    write_meta(
        &machine.runs_dir(),
        &meta_for(RUN_ID, RunStatus::Running, None),
    );
    h.call(3, "status", json!({ "run_id": RUN_ID })).await;
    let (_, _, structured) = h.call_result(3).await;
    assert_eq!(structured["status"], "waiting_input");
    assert_eq!(structured["live"], true);

    let mut unknown = meta_for("other-run", RunStatus::Starting, None);
    unknown.error = Some("e".to_string());
    write_meta(&machine.runs_dir(), &unknown);
    h.call(4, "status", json!({ "run_id": "other-run" })).await;
    let (_, text, structured) = h.call_result(4).await;
    assert_eq!(structured["status"], "starting");
    assert_eq!(structured["live"], false);
    assert!(text.contains("error: e"), "{text}");

    let (ready, _) = flaky_ready(usize::MAX);
    let mut h = Harness::start(
        daemon.client(),
        McpServeArgs::default(),
        machine.env(ready),
        fast_timing(),
    );
    h.call(5, "status", json!({ "run_id": RUN_ID })).await;
    let (_, _, structured) = h.call_result(5).await;
    assert_eq!(structured["status"], "running");
}

#[tokio::test]
async fn result_pages_the_answer_on_character_boundaries_and_caps_it_for_the_host() {
    let machine = Machine::new();
    let mut h = Harness::start(
        no_daemon_client(),
        McpServeArgs::default(),
        machine.env(always_ready()),
        fast_timing(),
    );
    h.call(1, "result", json!({ "run_id": "nope" })).await;
    let (is_error, text, _) = h.call_result(1).await;
    assert!(is_error);
    assert!(text.contains("no run 'nope'"), "{text}");

    write_run(&machine.runs_dir(), "silent", RunStatus::Complete, None);
    h.call(2, "result", json!({ "run_id": "silent" })).await;
    let (is_error, text, structured) = h.call_result(2).await;
    assert!(is_error);
    assert!(
        text.contains("has no final output (status: complete)"),
        "{text}"
    );
    assert_eq!(structured["status"], "complete");

    // "héllo wörld": é and ö are two bytes each.
    write_run(
        &machine.runs_dir(),
        RUN_ID,
        RunStatus::Complete,
        Some("héllo wörld"),
    );
    h.call(3, "result", json!({ "run_id": RUN_ID })).await;
    let (is_error, text, structured) = h.call_result(3).await;
    assert!(!is_error);
    assert_eq!(text, "héllo wörld");
    assert_eq!(structured["next_offset"], Value::Null);
    assert_eq!(structured["total_bytes"], 13);
    assert_eq!(structured["final_output"]["format"], "markdown");

    // An offset inside é backs off to its start; a page ending inside ö too.
    h.call(
        4,
        "result",
        json!({ "run_id": RUN_ID, "offset": 2, "max_bytes": 7 }),
    )
    .await;
    let (_, text, structured) = h.call_result(4).await;
    assert_eq!(text, "éllo w");
    assert_eq!(structured["offset"], 1);
    assert_eq!(structured["bytes"], 7);
    assert_eq!(structured["next_offset"], 8);

    h.call(5, "result", json!({ "run_id": RUN_ID, "offset": 500 }))
        .await;
    let (_, text, structured) = h.call_result(5).await;
    assert_eq!(text, "");
    assert_eq!(structured["next_offset"], Value::Null);

    // A `max_bytes` above what one result carries is clamped to it, so the
    // page is never cut after the fact and `bytes`/`next_offset` describe
    // what the host received. One ASCII byte then two-byte characters puts
    // every boundary at an odd offset, and the cap is even.
    let long = format!("x{}", "é".repeat(MCP_TEXT_CAP));
    write_run(
        &machine.runs_dir(),
        "long",
        RunStatus::Complete,
        Some(&long),
    );
    h.call(
        6,
        "result",
        json!({ "run_id": "long", "max_bytes": u64::MAX }),
    )
    .await;
    let (_, text, structured) = h.call_result(6).await;
    assert_eq!(text.len(), MCP_TEXT_CAP - 1, "cut off a character boundary");
    assert!(text.chars().skip(1).all(|c| c == 'é'));
    assert_eq!(structured["bytes"], MCP_TEXT_CAP - 1);
    assert_eq!(structured["next_offset"], MCP_TEXT_CAP - 1);
    assert!(structured.get("host_truncated").is_none());
    assert_eq!(structured["final_output"]["host_truncated"], false);
    // The next page picks up exactly where that one ended.
    h.call(
        7,
        "result",
        json!({ "run_id": "long", "offset": MCP_TEXT_CAP - 1 }),
    )
    .await;
    let (_, text, structured) = h.call_result(7).await;
    assert_eq!(structured["offset"], MCP_TEXT_CAP - 1);
    assert_eq!(text.len(), MCP_TEXT_CAP);
    assert!(text.chars().all(|c| c == 'é'));
    assert_eq!(structured["next_offset"], 2 * MCP_TEXT_CAP - 1);
    assert!(structured.get("host_truncated").is_none());
}

#[test]
fn cap_text_cuts_on_a_character_boundary_and_names_the_location_when_it_has_one() {
    let short = "x".repeat(MCP_TEXT_CAP);
    assert_eq!(cap_text(&short, None), (short.clone(), false));
    let long = "é".repeat(MCP_TEXT_CAP);
    let (cut, truncated) = cap_text(&long, None);
    assert!(truncated);
    assert!(cut.ends_with("\noutput truncated for the host"), "{cut}");
    let (cut, _) = cap_text(&long, Some("/runs/r/final_output"));
    assert!(cut.ends_with("or at /runs/r/final_output"), "{cut}");
}

#[test]
fn a_daemon_label_is_translated_and_an_unknown_one_passed_through() {
    assert_eq!(wire_status("active"), "running");
    assert_eq!(wire_status("waiting"), "waiting_input");
    assert_eq!(wire_status("complete"), "complete");
    assert_eq!(wire_status("hibernating"), "hibernating");
}

// ─── cancel / message / respond ───────────────────────────────────────────────

#[tokio::test]
async fn cancel_asks_the_daemon_and_falls_back_to_the_record_on_disk() {
    let machine = Machine::new();
    let daemon = ScriptedDaemon::new(vec![], |req| match req {
        ControlRequest::Cancel { run_id } if run_id == "live" => ControlResponse::Ok { ok: true },
        ControlRequest::Cancel { run_id } if run_id == "odd" => {
            ControlResponse::Status { status: None }
        }
        _ => ControlResponse::Ok { ok: false },
    });
    let mut h = Harness::usual(&daemon, &machine);
    h.call(1, "cancel", json!({ "run_id": "live" })).await;
    let (is_error, text, structured) = h.call_result(1).await;
    assert!(!is_error);
    assert!(text.contains("cancelled run live"), "{text}");
    assert_eq!(structured["cancelled"], true);

    h.call(2, "cancel", json!({ "run_id": "gone" })).await;
    let (is_error, text, _) = h.call_result(2).await;
    assert!(is_error);
    assert!(text.contains("no run 'gone'"), "{text}");

    h.call(3, "cancel", json!({ "run_id": "odd" })).await;
    let (is_error, text, _) = h.call_result(3).await;
    assert!(is_error);
    assert!(text.contains("unexpected daemon response"), "{text}");

    // No daemon: the record on disk is the fallback, in every state it can be.
    let mut h = Harness::start(
        no_daemon_client(),
        McpServeArgs::default(),
        machine.env(always_ready()),
        fast_timing(),
    );
    write_meta(
        &machine.runs_dir(),
        &meta_for(RUN_ID, RunStatus::Running, None),
    );
    h.call(4, "cancel", json!({ "run_id": RUN_ID })).await;
    let (is_error, text, structured) = h.call_result(4).await;
    assert!(!is_error);
    assert!(
        text.contains("on disk (the daemon did not answer"),
        "{text}"
    );
    assert_eq!(structured["on_disk"], true);

    h.call(5, "cancel", json!({ "run_id": RUN_ID })).await;
    let (is_error, text, structured) = h.call_result(5).await;
    assert!(!is_error);
    assert!(text.contains("had already finished"), "{text}");
    assert_eq!(structured["already_finished"], true);

    h.call(6, "cancel", json!({ "run_id": "never" })).await;
    let (is_error, text, _) = h.call_result(6).await;
    assert!(is_error);
    assert!(text.contains("there is no run 'never' on disk"), "{text}");

    // A record that cannot be rewritten: `meta.json` is a directory.
    std::fs::create_dir_all(machine.runs_dir().join("stuck").join("meta.json")).unwrap();
    h.call(7, "cancel", json!({ "run_id": "stuck" })).await;
    let (is_error, text, _) = h.call_result(7).await;
    assert!(is_error);
    assert!(text.contains("could not be rewritten"), "{text}");

    // The daemon will not even start: same fallback.
    let (ready, _) = flaky_ready(usize::MAX);
    let mut h = Harness::start(
        daemon.client(),
        McpServeArgs::default(),
        machine.env(ready),
        fast_timing(),
    );
    write_meta(
        &machine.runs_dir(),
        &meta_for("fresh", RunStatus::Running, None),
    );
    h.call(8, "cancel", json!({ "run_id": "fresh" })).await;
    let (is_error, text, _) = h.call_result(8).await;
    assert!(!is_error);
    assert!(text.contains("did not start the daemon"), "{text}");
}

#[tokio::test]
async fn message_reports_delivery_refusal_and_an_absent_daemon() {
    let machine = Machine::new();
    let daemon = ScriptedDaemon::new(vec![], |req| match req {
        ControlRequest::Message {
            agent_id,
            target_region,
            ..
        } if agent_id == "live" => ControlResponse::Ok {
            ok: target_region.as_deref() == Some("notes"),
        },
        _ => ControlResponse::Spawned {
            run_id: "x".to_string(),
        },
    });
    let mut h = Harness::usual(&daemon, &machine);
    h.call(
        1,
        "message",
        json!({ "run_id": "live", "content": "hi", "target_region": "notes" }),
    )
    .await;
    let (is_error, text, structured) = h.call_result(1).await;
    assert!(!is_error);
    assert!(text.contains("message delivered to run live"), "{text}");
    assert_eq!(structured["delivered"], true);

    h.call(2, "message", json!({ "run_id": "live", "content": "hi" }))
        .await;
    let (is_error, text, _) = h.call_result(2).await;
    assert!(is_error);
    assert!(text.contains("not accepting messages"), "{text}");

    h.call(3, "message", json!({ "run_id": "other", "content": "hi" }))
        .await;
    let (is_error, text, _) = h.call_result(3).await;
    assert!(is_error);
    assert!(text.contains("unexpected daemon response"), "{text}");

    h.call(4, "message", json!({ "run_id": "live" })).await;
    let (_, msg) = h.response(4).await;
    assert!(msg.error.unwrap().message.contains("content"));

    let mut h = Harness::start(
        no_daemon_client(),
        McpServeArgs::default(),
        machine.env(always_ready()),
        fast_timing(),
    );
    h.call(5, "message", json!({ "run_id": "live", "content": "hi" }))
        .await;
    let (is_error, text, _) = h.call_result(5).await;
    assert!(is_error);
    assert!(text.contains("not reachable"), "{text}");

    let (ready, _) = flaky_ready(usize::MAX);
    let mut h = Harness::start(
        daemon.client(),
        McpServeArgs::default(),
        machine.env(ready),
        fast_timing(),
    );
    h.call(6, "message", json!({ "run_id": "live", "content": "hi" }))
        .await;
    let (is_error, text, _) = h.call_result(6).await;
    assert!(is_error);
    assert!(text.contains("not available"), "{text}");
}

#[tokio::test]
async fn respond_lists_open_interactions_and_answers_one() {
    let machine = Machine::new();
    let open = std::sync::Arc::new(std::sync::Mutex::new(true));
    let toggle = open.clone();
    let daemon = ScriptedDaemon::new(vec![], move |req| match req {
        ControlRequest::ListInteractions => {
            let listed = *toggle.lock().unwrap();
            *toggle.lock().unwrap() = false;
            let WorldEvent::Interaction { request, .. } = interaction_for(RUN_ID, "q-1") else {
                unreachable!()
            };
            ControlResponse::Interactions {
                interactions: match listed {
                    true => vec![(RUN_ID.to_string(), request)],
                    false => vec![],
                },
            }
        }
        ControlRequest::AnswerInteraction { response } => ControlResponse::Ok {
            ok: response.request_id == "q-1",
        },
        _ => ControlResponse::Ok { ok: true },
    });
    let mut h = Harness::usual(&daemon, &machine);
    h.call(1, "respond", json!({})).await;
    let (is_error, text, structured) = h.call_result(1).await;
    assert!(!is_error);
    assert!(text.contains("request_id=q-1"), "{text}");
    assert_eq!(structured["interactions"][0]["run_id"], RUN_ID);
    assert_eq!(structured["interactions"][0]["request_id"], "q-1");

    h.call(2, "respond", json!({})).await;
    let (_, text, structured) = h.call_result(2).await;
    assert_eq!(text, "no open interactions");
    assert_eq!(structured["interactions"], json!([]));

    h.call(
        3,
        "respond",
        json!({ "request_id": "q-1", "approved": true, "scope": "session" }),
    )
    .await;
    let (is_error, text, structured) = h.call_result(3).await;
    assert!(!is_error);
    assert!(text.contains("answered interaction q-1"), "{text}");
    assert_eq!(structured["answered"], true);

    h.call(
        4,
        "respond",
        json!({ "request_id": "q-9", "value": "blue" }),
    )
    .await;
    let (is_error, text, _) = h.call_result(4).await;
    assert!(is_error);
    assert!(text.contains("no open interaction 'q-9'"), "{text}");

    h.call(
        5,
        "respond",
        json!({ "request_id": "q-1", "approved": true, "feedback": "no" }),
    )
    .await;
    let (_, msg) = h.response(5).await;
    assert!(msg.error.unwrap().message.contains("`feedback` goes with"));

    let answers: Vec<leviath_core::interaction::InteractionResponse> = daemon
        .requests()
        .into_iter()
        .filter_map(|r| match r {
            ControlRequest::AnswerInteraction { response } => Some(response),
            _ => None,
        })
        .collect();
    assert_eq!(answers.len(), 2);
    assert_eq!(answers[0].approved, Some(true));
    assert_eq!(
        answers[0].scope,
        Some(leviath_core::interaction::ApprovalScope::Run)
    );
    assert_eq!(answers[1].value.as_deref(), Some("blue"));

    // A listing needs the daemon; without it the tool says so.
    let mut h = Harness::start(
        no_daemon_client(),
        McpServeArgs::default(),
        machine.env(always_ready()),
        fast_timing(),
    );
    h.call(6, "respond", json!({})).await;
    let (is_error, text, _) = h.call_result(6).await;
    assert!(is_error);
    assert!(text.contains("not reachable"), "{text}");
    let (ready, _) = flaky_ready(usize::MAX);
    let mut h = Harness::start(
        daemon.client(),
        McpServeArgs::default(),
        machine.env(ready),
        fast_timing(),
    );
    h.call(7, "respond", json!({})).await;
    let (is_error, text, _) = h.call_result(7).await;
    assert!(is_error);
    assert!(text.contains("not available"), "{text}");

    // A daemon answering a listing with the wrong shape.
    let odd = ScriptedDaemon::new(vec![], |_| ControlResponse::Ok { ok: true });
    let mut h = Harness::usual(&odd, &machine);
    h.call(8, "respond", json!({})).await;
    let (is_error, text, _) = h.call_result(8).await;
    assert!(is_error);
    assert!(text.contains("unexpected daemon response"), "{text}");
}

#[test]
fn an_interaction_answer_is_built_the_way_lev_respond_builds_it() {
    use leviath_core::interaction::{ApprovalScope, InteractionResponse};
    let args = |v: Value| v.as_object().unwrap().clone();
    let built = |v: Value| build_interaction_response("r", &args(v));
    assert_eq!(
        built(json!({ "approved": false, "scope": "stage" })).unwrap(),
        InteractionResponse::approval("r", false, ApprovalScope::Stage)
    );
    assert_eq!(
        built(json!({ "approved": true, "scope": "once" })).unwrap(),
        InteractionResponse::approval("r", true, ApprovalScope::Once)
    );
    assert_eq!(
        built(json!({ "approved": true, "scope": "session" })).unwrap(),
        InteractionResponse::approval("r", true, ApprovalScope::Run)
    );
    assert_eq!(
        built(json!({ "approved": false, "feedback": "use ls" })).unwrap(),
        InteractionResponse::deny_with_feedback("r", "use ls")
    );
    assert_eq!(
        built(json!({ "choice_index": 2 })).unwrap(),
        InteractionResponse::choice("r", 2)
    );
    assert_eq!(
        built(json!({})).unwrap(),
        InteractionResponse::text("r", "")
    );
    // Types and enums are the schema's job upstream; here they read as absent.
    assert_eq!(
        built(json!({ "approved": true, "scope": "forever" })).unwrap(),
        InteractionResponse::approval("r", true, ApprovalScope::Once)
    );
    let err = built(json!({ "approved": true, "feedback": "no" })).unwrap_err();
    assert!(err.contains("`feedback` goes with"), "{err}");
}

// ─── list_runs / list_agents ──────────────────────────────────────────────────

#[tokio::test]
async fn list_runs_merges_the_daemons_view_with_the_records_on_disk() {
    let machine = Machine::new();
    let entry = |run_id: &str, started_at: i64| leviath_runtime::host::RunListEntry {
        run_id: run_id.to_string(),
        title: Some(format!("title of {run_id}")),
        status: leviath_runtime::components::AgentStatus::Active,
        wait_reason: None,
        stage: "implement".to_string(),
        stage_index: None,
        num_stages: None,
        iteration: 1,
        tool_calls: 0,
        last_progress_at: None,
        started_at: Some(started_at),
        active: None,
        unattended: true,
        empty_output: false,
        splits_degraded: 0,
        broken_scripts: vec![],
        read_paths: None,
        has_final_output: false,
    };
    let daemon = ScriptedDaemon::new(vec![], move |_| ControlResponse::List {
        runs: vec![entry("live-1", 300)],
        finished: vec![entry("done-1", 200)],
        health: Default::default(),
    });
    // On disk: one the daemon also lists, one it does not.
    write_meta(
        &machine.runs_dir(),
        &meta_for("done-1", RunStatus::Complete, None),
    );
    let mut old = meta_for("old-1", RunStatus::Cancelled, None);
    old.started_at = 100;
    write_meta(&machine.runs_dir(), &old);
    let mut h = Harness::usual(&daemon, &machine);
    h.call(1, "list_runs", json!({})).await;
    let (is_error, text, structured) = h.call_result(1).await;
    assert!(!is_error);
    assert_eq!(structured["daemon_reachable"], true);
    let ids: Vec<&str> = structured["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["run_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["live-1", "done-1", "old-1"]);
    assert_eq!(structured["runs"][0]["status"], "running");
    assert_eq!(structured["runs"][0]["live"], true);
    assert_eq!(structured["runs"][2]["status"], "cancelled");
    assert_eq!(structured["runs"][2]["live"], false);
    assert!(text.contains("title of live-1"), "{text}");

    h.call(
        2,
        "list_runs",
        json!({ "limit": 1, "include_finished_on_disk": false }),
    )
    .await;
    let (_, _, structured) = h.call_result(2).await;
    assert_eq!(structured["runs"].as_array().unwrap().len(), 1);

    let mut h = Harness::start(
        no_daemon_client(),
        McpServeArgs::default(),
        machine.env(always_ready()),
        fast_timing(),
    );
    h.call(3, "list_runs", json!({})).await;
    let (is_error, text, structured) = h.call_result(3).await;
    assert!(!is_error);
    assert_eq!(structured["daemon_reachable"], false);
    assert!(text.contains("only runs on disk are listed"), "{text}");
    let ids: Vec<&str> = structured["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["run_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["done-1", "old-1"]);

    let (ready, _) = flaky_ready(usize::MAX);
    let mut h = Harness::start(
        daemon.client(),
        McpServeArgs::default(),
        machine.env(ready),
        fast_timing(),
    );
    h.call(4, "list_runs", json!({ "include_finished_on_disk": false }))
        .await;
    let (_, text, structured) = h.call_result(4).await;
    assert_eq!(structured["runs"], json!([]));
    assert!(text.starts_with("no runs"), "{text}");
}

#[tokio::test]
async fn list_agents_shows_installed_blueprints_and_bundled_ones_not_yet_installed() {
    let machine = Machine::new();
    let mut h = Harness::start(
        no_daemon_client(),
        McpServeArgs::default(),
        machine.env(always_ready()),
        fast_timing(),
    );
    // Nothing installed yet: every bundled agent is offered.
    h.call(1, "list_agents", json!({})).await;
    let (is_error, text, structured) = h.call_result(1).await;
    assert!(!is_error);
    let agents = structured["agents"].as_array().unwrap();
    assert_eq!(agents.len(), crate::bundled::BUNDLED_AGENTS.len());
    assert!(
        agents
            .iter()
            .all(|a| a["installed"] == false && a["bundled"] == true)
    );
    let coder = agents.iter().find(|a| a["name"] == "coder").unwrap();
    assert!(!coder["description"].as_str().unwrap().is_empty());
    assert!(text.contains("coder v"), "{text}");
    assert!(text.contains("(bundled, installed on demand)"), "{text}");
    assert_eq!(structured["cwd_has_manifest"], false);
    assert_eq!(structured["default_agent"], "orchestrator");

    machine.install_agent("coder", "");
    machine.install_agent("mine", "");
    std::fs::create_dir_all(machine.agents_dir().join("broken")).unwrap();
    std::fs::write(
        machine.agents_dir().join("broken").join("agent.leviath"),
        "not = [toml",
    )
    .unwrap();
    std::fs::create_dir_all(machine.agents_dir().join("not-an-agent")).unwrap();
    std::fs::write(
        machine.project().join("agent.leviath"),
        crate::test_support::inline_coder_manifest(),
    )
    .unwrap();
    h.call(2, "list_agents", json!({})).await;
    let (_, text, structured) = h.call_result(2).await;
    let agents = structured["agents"].as_array().unwrap();
    let coder = agents.iter().find(|a| a["name"] == "coder").unwrap();
    assert_eq!(coder["installed"], true);
    assert_eq!(coder["bundled"], true);
    assert_eq!(coder["accepts_task"], true);
    assert_eq!(coder["version"], "0.0.0");
    let mine = agents.iter().find(|a| a["name"] == "mine").unwrap();
    assert_eq!(mine["bundled"], false);
    assert_eq!(mine["caller_inputs"], json!(["task"]));
    let broken = agents.iter().find(|a| a["name"] == "broken").unwrap();
    assert!(broken["error"].as_str().unwrap().contains("does not parse"));
    assert!(agents.iter().all(|a| a["name"] != "not-an-agent"));
    assert_eq!(structured["cwd_has_manifest"], true);
    assert!(text.contains("holds an agent.leviath"), "{text}");
    assert!(text.contains("coder v0.0.0 (bundled):"), "{text}");
    assert!(text.contains("broken v?: does not parse"), "{text}");

    // A listing longer than the host cap is cut and says so, even though a
    // listing has no final output to point at.
    let wordy = machine.agents_dir().join("wordy");
    std::fs::create_dir_all(&wordy).unwrap();
    std::fs::write(
        wordy.join("agent.leviath"),
        crate::test_support::inline_coder_manifest().replace(
            "Inline test blueprint (coder-shaped); self-contained.",
            &"w".repeat(MCP_TEXT_CAP),
        ),
    )
    .unwrap();
    h.call(5, "list_agents", json!({})).await;
    let (_, text, structured) = h.call_result(5).await;
    assert!(
        text.ends_with("\noutput truncated for the host"),
        "{}",
        text.len()
    );
    assert_eq!(structured["host_truncated"], true);
    std::fs::remove_dir_all(&wordy).unwrap();

    // No agents dir at all, or one that does not exist: bundled only.
    let mut env = machine.env(always_ready());
    env.agents_dir = None;
    let mut h = Harness::start(
        no_daemon_client(),
        McpServeArgs::default(),
        env,
        fast_timing(),
    );
    h.call(3, "list_agents", json!({})).await;
    let (_, _, structured) = h.call_result(3).await;
    assert!(
        structured["agents"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["installed"] == false)
    );
    let mut env = machine.env(always_ready());
    env.agents_dir = Some(machine.path.join("no-such-dir"));
    let mut h = Harness::start(
        no_daemon_client(),
        McpServeArgs::default(),
        env,
        fast_timing(),
    );
    h.call(4, "list_agents", json!({})).await;
    let (_, _, structured) = h.call_result(4).await;
    assert_eq!(
        structured["agents"].as_array().unwrap().len(),
        crate::bundled::BUNDLED_AGENTS.len()
    );
}

// ─── install_tool / list_tools ────────────────────────────────────────────────

const UPPER: &str = "// @tool upper\n// @description Upper-case text\n// @param text string required \"input\"\nparams.text.to_upper()\n";

#[tokio::test]
async fn install_tool_writes_a_stamped_script_that_list_tools_then_shows() {
    let machine = Machine::new();
    let mut h = Harness::start(
        no_daemon_client(),
        McpServeArgs::default(),
        machine.env(always_ready()),
        fast_timing(),
    );
    h.call(1, "list_tools", json!({})).await;
    let (_, text, structured) = h.call_result(1).await;
    assert_eq!(text, "no global Rhai tools installed");
    assert_eq!(structured["tools"], json!([]));

    h.call(
        2,
        "install_tool",
        json!({ "name": "upper", "source": UPPER }),
    )
    .await;
    let (is_error, text, structured) = h.call_result(2).await;
    assert!(!is_error, "{text}");
    assert!(text.contains("Installed tool 'upper'"), "{text}");
    assert_eq!(structured["name"], "upper");
    assert_eq!(structured["replaced"], false);
    let written = std::fs::read_to_string(machine.tools_dir().join("upper.rhai")).unwrap();
    assert!(
        written.starts_with("// installed by leviath: mcp host, workdir "),
        "{written}"
    );

    h.call(
        3,
        "install_tool",
        json!({ "name": "upper", "source": UPPER }),
    )
    .await;
    let (is_error, text, _) = h.call_result(3).await;
    assert!(is_error);
    assert!(text.contains("already exists"), "{text}");
    h.call(
        4,
        "install_tool",
        json!({ "name": "upper", "source": UPPER, "overwrite": true }),
    )
    .await;
    let (_, _, structured) = h.call_result(4).await;
    assert_eq!(structured["replaced"], true);

    // Built-in and sub-agent names are reserved.
    let src = UPPER.replace("@tool upper", "@tool spawn_agent");
    h.call(
        5,
        "install_tool",
        json!({ "name": "spawn_agent", "source": src }),
    )
    .await;
    let (is_error, text, _) = h.call_result(5).await;
    assert!(is_error);
    assert!(text.contains("built-in tool"), "{text}");

    // A hand-written script without a stamp, and a file that does not compile.
    std::fs::write(
        machine.tools_dir().join("plain.rhai"),
        "// @tool plain\n// @description Plain\n42\n",
    )
    .unwrap();
    std::fs::write(
        machine.tools_dir().join("bad.rhai"),
        "// @tool bad\nlet x = ;\n",
    )
    .unwrap();
    h.call(6, "list_tools", json!({})).await;
    let (_, text, structured) = h.call_result(6).await;
    let tools = structured["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], "plain");
    assert_eq!(tools[0]["provenance"], Value::Null);
    assert_eq!(tools[1]["name"], "upper");
    assert!(
        tools[1]["provenance"]
            .as_str()
            .unwrap()
            .starts_with("// installed by leviath:")
    );
    assert_eq!(tools[1]["params"], "text:string!");
    assert_eq!(structured["skipped"].as_array().unwrap().len(), 1);
    assert!(
        text.contains("upper(text:string!): Upper-case text  [// installed by leviath:"),
        "{text}"
    );
    assert!(text.contains("skipped "), "{text}");

    // No home: nothing to install into, nothing to list.
    let mut env = machine.env(always_ready());
    env.tools_dir = None;
    let mut h = Harness::start(
        no_daemon_client(),
        McpServeArgs::default(),
        env,
        fast_timing(),
    );
    h.call(
        7,
        "install_tool",
        json!({ "name": "upper", "source": UPPER }),
    )
    .await;
    let (is_error, text, _) = h.call_result(7).await;
    assert!(is_error);
    assert!(text.contains("LEVIATH_HOME"), "{text}");
    h.call(8, "list_tools", json!({})).await;
    let (_, _, structured) = h.call_result(8).await;
    assert_eq!(structured["dir"], Value::Null);
}
