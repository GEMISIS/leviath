//! Stdio transport: newline-delimited JSON-RPC over a child process's pipes.
//!
//! This is the transport nearly every locally-installed MCP server uses
//! (`npx …`, `uvx …`, `docker run …`).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout};

use super::Transport;
use super::jsonrpc::{self, Inbound, JsonRpcRequest, JsonRpcResponse};

/// How many trailing stderr lines to keep for diagnostics.
const STDERR_TAIL_LINES: usize = 20;

/// A bounded, shared tail of the child's stderr.
///
/// A server that fails to start writes its reason to stderr and exits.
/// Sending that to `/dev/null` leaves the user a bare "connection closed" and
/// no hint of the missing package, bad flag, or absent runtime that actually
/// caused it. Bounded so a chatty server can't grow this without limit.
#[derive(Clone, Default)]
pub(crate) struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    fn push(&self, line: String) {
        // Poisoning would mean a panic while holding the lock; the only code
        // under it is a push/pop pair, so recover rather than propagate.
        let mut buf = leviath_core::sync::lock(&self.0);
        if buf.len() == STDERR_TAIL_LINES {
            buf.pop_front();
        }
        buf.push_back(line);
    }

    /// The captured lines, newest last, or `None` if the server said nothing.
    pub(crate) fn snapshot(&self) -> Option<String> {
        let buf = leviath_core::sync::lock(&self.0);
        if buf.is_empty() {
            return None;
        }
        Some(buf.iter().cloned().collect::<Vec<_>>().join("\n"))
    }
}

/// Executable names to try for `command`, in order.
///
/// On Windows the npm-family launchers (`npx`, `npm`, `yarn`, `pnpm`) exist
/// only as `.cmd` shims, and `CreateProcess` will not find them from the bare
/// name - so `command = "npx"`, far and away the most common MCP server config
/// in the wild, simply fails to spawn there. Trying the executable suffixes
/// fixes that.
///
/// `windows` is a parameter rather than a `#[cfg]` so both branches are real,
/// compiled, tested code on every platform.
fn command_candidates_for(command: &str, windows: bool) -> Vec<String> {
    let mut candidates = vec![command.to_string()];
    // An explicit extension or a path means the caller already told us exactly
    // what to run; second-guessing it would be wrong.
    let already_qualified =
        command.contains('.') || command.contains('/') || command.contains('\\');
    if windows && !already_qualified {
        for suffix in [".cmd", ".exe", ".bat"] {
            candidates.push(format!("{command}{suffix}"));
        }
    }
    candidates
}

/// [`command_candidates_for`] against the host platform.
fn command_candidates(command: &str) -> Vec<String> {
    command_candidates_for(command, cfg!(windows))
}

/// Build a clean environment for a spawned MCP server.
///
/// **Allowlist, not denylist.** An MCP server is third-party code by definition,
/// and we are choosing what to hand it - so the question is "what does it need",
/// not "what must we remember to withhold". The previous substring denylist
/// (`API_KEY`, `SECRET_KEY`, `ACCESS_TOKEN`, …) passed everything whose name
/// happened not to match: `AWS_SECRET_ACCESS_KEY` matches neither `API_SECRET`
/// nor `SECRET_KEY`, and `GITHUB_TOKEN`, `GH_TOKEN`, `NPM_TOKEN`,
/// `DATABASE_URL`, `SSH_AUTH_SOCK` and Leviath's own `LEVIATH_API_TOKEN` were
/// never on the list at all. A denylist here loses to every credential the
/// ecosystem invents next.
///
/// What survives is [`leviath_core::child_env_allowed`]: enough to find an
/// interpreter and behave like a terminal program. Anything else a server
/// legitimately needs is declared in its own `env` block in config, which the
/// caller applies *after* this filter and which therefore always wins - that is
/// the supported way to give a server its token.
pub fn filter_env(vars: &[(String, String)]) -> HashMap<String, String> {
    vars.iter()
        .filter(|(key, _)| leviath_core::child_env_allowed(key))
        .cloned()
        .collect()
}

/// Newline-delimited JSON-RPC over a spawned server's stdin/stdout.
pub(crate) struct StdioTransport {
    child: Child,
    writer: BufWriter<ChildStdin>,
    reader: BufReader<ChildStdout>,
    stderr: StderrTail,
}

impl StdioTransport {
    /// Spawn an MCP server as a child process.
    pub(crate) async fn spawn(
        command: &str,
        args: &[&str],
        env: &HashMap<String, String>,
    ) -> anyhow::Result<Self> {
        tracing::info!(command = %command, "Spawning MCP server process");

        let candidates = command_candidates(command);
        let mut last_err = None;
        for candidate in &candidates {
            match Self::try_spawn(candidate, args, env) {
                Ok(child) => return Ok(Self::from_child(child)),
                Err(e) => last_err = Some(e),
            }
        }

        let err = last_err.expect("command_candidates always yields at least the bare name");
        Err(anyhow::anyhow!(
            "Failed to spawn MCP server '{}': {}",
            command,
            err
        ))
    }

    /// One spawn attempt with a concrete executable name.
    fn try_spawn(
        command: &str,
        args: &[&str],
        env: &HashMap<String, String>,
    ) -> std::io::Result<Child> {
        let mut cmd = leviath_sys::child_command_async(command);

        // Build a clean environment, then layer the explicitly configured vars
        // (intentional, from MCP config) on top.
        cmd.env_clear()
            .envs(filter_env(&std::env::vars().collect::<Vec<_>>()));
        cmd.envs(env);

        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // The server talks JSON-RPC over those pipes and has no use for a
        // console. It matters most here: `command_candidates` resolves `npx` to
        // `npx.cmd`, a batch file, which Windows always runs through `cmd.exe`.

        cmd.spawn()
    }

    /// Wire up a freshly spawned child, draining its stderr into the tail.
    fn from_child(mut child: Child) -> Self {
        let stdin = child.stdin.take().expect("stdin piped at spawn");
        let stdout = child.stdout.take().expect("stdout piped at spawn");
        let stderr_pipe = child.stderr.take().expect("stderr piped at spawn");

        let stderr = StderrTail::default();
        let sink = stderr.clone();
        // Drained continuously: an unread stderr pipe fills its buffer and then
        // blocks the server mid-write, which looks exactly like a hang.
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr_pipe).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(line = %line, "MCP server stderr");
                sink.push(line);
            }
        });

        Self {
            child,
            writer: BufWriter::new(stdin),
            reader: BufReader::new(stdout),
            stderr,
        }
    }

    /// Attach whatever the server wrote to stderr to `context`.
    ///
    /// This is the difference between "MCP server closed connection" and a
    /// message that names the missing package.
    fn with_stderr(&self, context: String) -> anyhow::Error {
        match self.stderr.snapshot() {
            Some(tail) => anyhow::anyhow!("{context}\nserver stderr:\n{tail}"),
            None => anyhow::anyhow!(context),
        }
    }

    /// Read one frame from the server, or `Ok(None)` at end of stream.
    async fn read_frame(&mut self) -> anyhow::Result<Option<Value>> {
        let mut line = String::new();
        // Propagated, never unwrapped: an `.expect()` here turns a server dying
        // mid-read into a whole-daemon panic rather than one failed call.
        // `read_line` also fails outright on non-UTF-8 output, which a server
        // writing raw bytes or a mis-encoded log line really does produce.
        let read = self
            .reader
            .read_line(&mut line)
            .await
            .map_err(|e| self.with_stderr(format!("Failed to read from MCP server: {e}")))?;

        if read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            // Blank keepalive line; not a frame.
            return Ok(Some(Value::Null));
        }
        let frame: Value = serde_json::from_str(trimmed)
            .map_err(|e| self.with_stderr(format!("Failed to parse JSON-RPC response: {e}")))?;
        Ok(Some(frame))
    }

    /// Read frames until one is a response to us, answering anything the
    /// server asks along the way.
    async fn read_until_response(&mut self) -> anyhow::Result<JsonRpcResponse> {
        loop {
            let Some(frame) = self.read_frame().await? else {
                return Err(
                    self.with_stderr("MCP server closed connection unexpectedly".to_string())
                );
            };
            if frame.is_null() {
                continue;
            }

            match jsonrpc::classify(frame).map_err(|e| self.with_stderr(e.to_string()))? {
                Inbound::Response(response) => return Ok(*response),
                Inbound::ServerRequest { id, method } => {
                    tracing::debug!(method = %method, "Answering server-initiated request");
                    let reply = jsonrpc::reply_to_server_request(&id, &method);
                    let mut line = reply.to_string();
                    line.push('\n');
                    Self::write_line(
                        &mut self.writer,
                        &line,
                        "Failed to write reply to MCP server",
                        "Failed to flush reply to MCP server",
                    )
                    .await?;
                }
                Inbound::Notification { method } => {
                    tracing::debug!(method = %method, "Ignoring server notification");
                }
            }
        }
    }

    /// Write `line` to `writer` and flush it, mapping I/O errors to context-
    /// tagged `anyhow` errors.
    ///
    /// Split out (behavior-preserving) so the write / flush error arms can be
    /// exercised against an injectable writer on every platform. The real
    /// `BufWriter<ChildStdin>` path buffers differently per OS (a >8KB write
    /// to a broken pipe surfaces the error in `write_all` on Unix but is
    /// absorbed by the OS pipe buffer on Windows, deferring it to `flush`), so
    /// a broken-pipe integration test can't deterministically hit the
    /// `write_all` error arm on Windows. `writer` is a trait object rather
    /// than `impl AsyncWrite` so production and each test share one
    /// monomorphization (avoids the generic coverage-attribution artifact).
    async fn write_line(
        writer: &mut (dyn AsyncWrite + Unpin + Send),
        line: &str,
        write_err: &str,
        flush_err: &str,
    ) -> anyhow::Result<()> {
        writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| anyhow::anyhow!("{}: {}", write_err, e))?;
        writer
            .flush()
            .await
            .map_err(|e| anyhow::anyhow!("{}: {}", flush_err, e))?;
        Ok(())
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn send_request(
        &mut self,
        req: &JsonRpcRequest,
        timeout: Duration,
    ) -> anyhow::Result<JsonRpcResponse> {
        tracing::trace!(method = %req.method, "Sending JSON-RPC request");

        Self::write_line(
            &mut self.writer,
            &req.to_line(),
            "Failed to write to MCP server stdin",
            "Failed to flush MCP server stdin",
        )
        .await?;

        // Bounded: a server that accepts the request and then goes silent used
        // to block the caller forever.
        match tokio::time::timeout(timeout, self.read_until_response()).await {
            Ok(result) => result,
            Err(_) => Err(self.with_stderr(format!(
                "MCP server did not respond to '{}' within {}s",
                req.method,
                timeout.as_secs()
            ))),
        }
    }

    async fn send_notification(&mut self, req: &JsonRpcRequest) -> anyhow::Result<()> {
        tracing::trace!(method = %req.method, "Sending JSON-RPC notification");
        Self::write_line(
            &mut self.writer,
            &req.to_line(),
            "Failed to write notification",
            "Failed to flush notification",
        )
        .await
    }

    async fn close(&mut self) -> anyhow::Result<()> {
        // Closing stdin is the graceful signal; the kill is the backstop for a
        // server that ignores it. Both are best-effort: a dead server must
        // never block cleanup.
        let _ = self.writer.shutdown().await;
        let _ = self.child.kill().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::always_on_tracing_guard;
    use crate::transport::DEFAULT_REQUEST_TIMEOUT;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    // ─── command_candidates ───────────────────────────────────────────────

    #[test]
    fn bare_command_is_unchanged_off_windows() {
        assert_eq!(command_candidates_for("npx", false), vec!["npx"]);
    }

    #[test]
    fn bare_command_gains_launcher_suffixes_on_windows() {
        // Without `.cmd`, `command = "npx"` - the single most common MCP
        // server config there is - fails to spawn on Windows.
        assert_eq!(
            command_candidates_for("npx", true),
            vec!["npx", "npx.cmd", "npx.exe", "npx.bat"]
        );
    }

    #[test]
    fn explicitly_qualified_commands_are_left_alone_on_windows() {
        // An extension or a path means the caller already said what to run.
        assert_eq!(command_candidates_for("node.exe", true), vec!["node.exe"]);
        assert_eq!(
            command_candidates_for("C:\\tools\\srv", true),
            vec!["C:\\tools\\srv"]
        );
        assert_eq!(
            command_candidates_for("/usr/bin/srv", true),
            vec!["/usr/bin/srv"]
        );
    }

    #[test]
    fn host_candidates_include_the_bare_command() {
        assert_eq!(command_candidates("python3")[0], "python3");
    }

    // ─── StderrTail ───────────────────────────────────────────────────────

    #[test]
    fn stderr_tail_is_empty_until_written() {
        assert!(StderrTail::default().snapshot().is_none());
    }

    #[test]
    fn stderr_tail_preserves_order() {
        let tail = StderrTail::default();
        tail.push("first".to_string());
        tail.push("second".to_string());
        assert_eq!(tail.snapshot().unwrap(), "first\nsecond");
    }

    #[test]
    fn stderr_tail_survives_a_poisoned_lock() {
        // The drain task holds this lock; if it ever panicked mid-push the
        // mutex would stay poisoned and every later diagnostic would panic
        // too - turning a server's error message into a second failure.
        let tail = StderrTail::default();
        let doomed = tail.clone();
        let _ = std::thread::spawn(move || {
            let _held = doomed.0.lock().expect("fresh lock");
            panic!("poisoning the stderr tail on purpose");
        })
        .join();

        tail.push("still works".to_string());
        assert_eq!(tail.snapshot().unwrap(), "still works");
    }

    #[test]
    fn stderr_tail_drops_oldest_beyond_the_cap() {
        let tail = StderrTail::default();
        for i in 0..(STDERR_TAIL_LINES + 5) {
            tail.push(format!("line{i}"));
        }
        let snapshot = tail.snapshot().unwrap();
        assert_eq!(snapshot.lines().count(), STDERR_TAIL_LINES);
        assert!(!snapshot.contains("line0"), "oldest should be evicted");
        assert!(snapshot.contains(&format!("line{}", STDERR_TAIL_LINES + 4)));
    }

    // ─── write_line error arms ────────────────────────────────────────────

    /// Configurable in-memory writer used to exercise `write_line`'s
    /// write/flush error arms deterministically on every platform (the real
    /// broken-pipe path can't reliably hit the write_all arm on Windows).
    struct FakeWriter {
        fail_write: bool,
        fail_flush: bool,
    }

    impl AsyncWrite for FakeWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.fail_write {
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "write boom",
                )))
            } else {
                Poll::Ready(Ok(buf.len()))
            }
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            if self.fail_flush {
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "flush boom",
                )))
            } else {
                Poll::Ready(Ok(()))
            }
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn write_line_maps_write_error() {
        let mut writer = FakeWriter {
            fail_write: true,
            fail_flush: false,
        };
        let err = StdioTransport::write_line(&mut writer, "payload\n", "WCTX", "FCTX")
            .await
            .expect_err("write should fail");
        let msg = err.to_string();
        assert!(msg.contains("WCTX"), "got: {msg}");
        assert!(msg.contains("write boom"), "got: {msg}");
    }

    #[tokio::test]
    async fn write_line_maps_flush_error() {
        let mut writer = FakeWriter {
            fail_write: false,
            fail_flush: true,
        };
        let err = StdioTransport::write_line(&mut writer, "payload\n", "WCTX", "FCTX")
            .await
            .expect_err("flush should fail");
        let msg = err.to_string();
        assert!(msg.contains("FCTX"), "got: {msg}");
        assert!(msg.contains("flush boom"), "got: {msg}");
    }

    #[tokio::test]
    async fn write_line_success_then_shutdown() {
        let mut writer = FakeWriter {
            fail_write: false,
            fail_flush: false,
        };
        // Exercises the write-OK + flush-OK arms; the trailing shutdown covers
        // poll_shutdown.
        StdioTransport::write_line(&mut writer, "payload\n", "WCTX", "FCTX")
            .await
            .expect("write should succeed");
        writer.shutdown().await.expect("shutdown should succeed");
    }

    // ─── filter_env ───────────────────────────────────────────────────────

    #[test]
    fn filter_env_strips_credential_shaped_keys() {
        let vars = [
            ("HOME", "/home/user"),
            ("PATH", "/usr/bin"),
            ("ANTHROPIC_API_KEY", "sk-ant-secret"),
            ("MY_PASSWORD", "hunter2"),
            ("DB_ACCESS_TOKEN", "tok"),
            ("SOME_AUTH_TOKEN", "auth"),
            ("SSH_PRIVATE_KEY", "key"),
            ("MY_API_SECRET", "sec"),
            ("SECRET_KEY_BASE", "skb"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()));

        let filtered = filter_env(&vars);

        assert_eq!(filtered.get("HOME").unwrap(), "/home/user");
        assert_eq!(filtered.get("PATH").unwrap(), "/usr/bin");
        assert_eq!(filtered.len(), 2, "only allowlisted names survive");
    }

    /// The allowlist excludes *unknown* names, not merely credential-shaped
    /// ones. `SAFE_VAR` is harmless and still does not reach the child: a server
    /// that needs it declares it in its own `env` block, which the caller applies
    /// after this filter. That is the whole difference between an allowlist and
    /// the denylist this replaced.
    #[test]
    fn filter_env_excludes_anything_not_on_the_allowlist() {
        let vars = [
            ("my_api_key", "secret"),
            ("CUSTOM_API_KEY_VALUE", "val"),
            ("SAFE_VAR", "ok"),
            ("PATH", "/usr/bin"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()));
        let filtered = filter_env(&vars);
        assert_eq!(filtered.len(), 1);
        assert!(filtered.contains_key("PATH"));
        assert!(!filtered.contains_key("SAFE_VAR"));
    }

    /// Names a substring denylist lets through. Each of these would reach
    /// every spawned MCP server - third-party code, by definition.
    #[test]
    fn filter_env_strips_what_the_old_denylist_missed() {
        let vars = [
            ("AWS_SECRET_ACCESS_KEY", "x"),
            ("AWS_SESSION_TOKEN", "x"),
            ("GITHUB_TOKEN", "x"),
            ("GH_TOKEN", "x"),
            ("NPM_TOKEN", "x"),
            ("DATABASE_URL", "x"),
            ("SSH_AUTH_SOCK", "x"),
            ("LEVIATH_API_TOKEN", "x"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()));
        assert!(filter_env(&vars).is_empty());
    }

    #[test]
    fn filter_env_of_nothing_is_nothing() {
        assert!(filter_env(&[]).is_empty());
    }

    // ─── spawn / request / notification against stub servers ──────────────

    /// A Python stub that answers `initialize` and then whatever `extra` adds.
    fn stub(extra: &str) -> String {
        format!(
            r#"
import sys, json
def respond(id_, result):
    sys.stdout.write(json.dumps({{"jsonrpc": "2.0", "id": id_, "result": result}}) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method, id_ = req.get("method", ""), req.get("id")
    if method == "initialize":
        respond(id_, {{"capabilities": {{}}, "protocolVersion": "2024-11-05"}})
{extra}
"#
        )
    }

    async fn spawn_stub(script: &str) -> StdioTransport {
        StdioTransport::spawn("python3", &["-c", script], &HashMap::new())
            .await
            .expect("stub should spawn")
    }

    fn init_request() -> JsonRpcRequest {
        JsonRpcRequest::request(1, "initialize", serde_json::json!({}))
    }

    #[tokio::test]
    async fn spawn_failure_reports_the_original_command_name() {
        let err = StdioTransport::spawn("/nonexistent/mcp/server", &[], &HashMap::new())
            .await
            .err()
            .expect("should not spawn");
        assert!(err.to_string().contains("/nonexistent/mcp/server"));
        assert!(err.to_string().contains("Failed to spawn"));
    }

    #[tokio::test]
    async fn request_response_roundtrip() {
        let _guard = always_on_tracing_guard();
        let mut t = spawn_stub(&stub("")).await;
        let response = t
            .send_request(&init_request(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .expect("initialize should succeed");
        assert!(response.into_result().is_ok());
    }

    #[tokio::test]
    async fn server_ping_is_answered_and_does_not_consume_our_response() {
        let _guard = always_on_tracing_guard();
        // The server pings us *before* answering. Treating that ping as our
        // response would desynchronize the whole conversation.
        let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    if req.get("method") == "initialize":
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": 99, "method": "ping"}) + "\n")
        sys.stdout.flush()
        # Wait for the client's reply before answering, so a client that never
        # replies hangs here and fails the test by timing out.
        reply = json.loads(sys.stdin.readline())
        assert reply["id"] == 99 and reply.get("result") == {}, reply
        sys.stdout.write(json.dumps(
            {"jsonrpc": "2.0", "id": req.get("id"), "result": {"pinged": True}}) + "\n")
        sys.stdout.flush()
"#;
        let mut t = spawn_stub(script).await;
        let value = t
            .send_request(&init_request(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .expect("request should survive an interleaved ping")
            .into_result()
            .unwrap();
        assert_eq!(value, serde_json::json!({"pinged": true}));
    }

    #[tokio::test]
    async fn unsupported_server_request_is_refused_not_ignored() {
        let _guard = always_on_tracing_guard();
        let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    if req.get("method") == "initialize":
        sys.stdout.write(json.dumps(
            {"jsonrpc": "2.0", "id": 5, "method": "sampling/createMessage"}) + "\n")
        sys.stdout.flush()
        reply = json.loads(sys.stdin.readline())
        assert reply["error"]["code"] == -32601, reply
        sys.stdout.write(json.dumps(
            {"jsonrpc": "2.0", "id": req.get("id"), "result": {"ok": True}}) + "\n")
        sys.stdout.flush()
"#;
        let mut t = spawn_stub(script).await;
        assert!(
            t.send_request(&init_request(), DEFAULT_REQUEST_TIMEOUT)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn server_notifications_and_blank_lines_are_skipped() {
        let _guard = always_on_tracing_guard();
        let script = r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    if req.get("method") == "initialize":
        sys.stdout.write("\n")
        sys.stdout.write(json.dumps(
            {"jsonrpc": "2.0", "method": "notifications/progress"}) + "\n")
        sys.stdout.write(json.dumps(
            {"jsonrpc": "2.0", "id": req.get("id"), "result": {"ok": True}}) + "\n")
        sys.stdout.flush()
"#;
        let mut t = spawn_stub(script).await;
        assert!(
            t.send_request(&init_request(), DEFAULT_REQUEST_TIMEOUT)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn silent_server_times_out_instead_of_hanging() {
        let _guard = always_on_tracing_guard();
        // Reads the request and then never answers.
        let script = "import sys, time\nsys.stdin.readline()\ntime.sleep(30)\n";
        let mut t = spawn_stub(script).await;
        let err = t
            .send_request(&init_request(), Duration::from_millis(200))
            .await
            .err()
            .expect("must time out, not hang");
        assert!(err.to_string().contains("did not respond"), "got: {err}");
    }

    #[tokio::test]
    async fn closed_connection_is_an_error_not_a_panic() {
        let _guard = always_on_tracing_guard();
        // Regression guard: an `.expect()` on the read turns a server dying
        // mid-request into a daemon panic rather than a failed call.
        let script = "import sys\nsys.stdin.readline()\nsys.stdout.close()\n";
        let mut t = spawn_stub(script).await;
        let err = t
            .send_request(&init_request(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("closed stdout should error");
        assert!(err.to_string().contains("closed connection"), "got: {err}");
    }

    #[tokio::test]
    async fn stderr_is_captured_into_the_error() {
        let _guard = always_on_tracing_guard();
        // The whole point of piping stderr: a server that dies on startup gets
        // to say why, instead of the user seeing a bare "closed connection".
        let script = r#"
import sys
sys.stderr.write("Cannot find module '@scope/mcp-server'\n")
sys.stderr.flush()
sys.stdin.readline()
sys.stdout.close()
"#;
        let mut t = spawn_stub(script).await;
        // Give the stderr drain task a moment to observe the line.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let err = t
            .send_request(&init_request(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("closed stdout should error");
        let msg = err.to_string();
        assert!(msg.contains("server stderr"), "got: {msg}");
        assert!(msg.contains("Cannot find module"), "got: {msg}");
    }

    #[tokio::test]
    async fn failing_to_answer_a_server_request_fails_the_call() {
        let _guard = always_on_tracing_guard();
        // The server closes its stdin *before* sending the ping, so our reply
        // write is guaranteed to hit a broken pipe. Ordering matters: closing
        // afterwards would race, because we can read the ping and start writing
        // while the child is still merely about to run its next line.
        let script = r#"
import sys, json, os, time
sys.stdin.readline()
os.close(0)
sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": 1, "method": "ping"}) + "\n")
sys.stdout.flush()
time.sleep(10)
"#;
        let mut t = spawn_stub(script).await;
        let result = t
            .send_request(&init_request(), DEFAULT_REQUEST_TIMEOUT)
            .await;
        // Not being able to reply means the connection is gone; surfacing that
        // beats silently continuing into a read that will fail anyway.
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn non_utf8_output_is_a_read_error_not_a_panic() {
        let _guard = always_on_tracing_guard();
        // `read_line` rejects invalid UTF-8 outright, so this exercises the
        // read-error arm (as opposed to a clean EOF). A server that writes raw
        // bytes or a mis-encoded log line to stdout really does hit this.
        let script = r#"
import sys
sys.stdin.readline()
sys.stdout.buffer.write(b"\xff\xfe not utf8\n")
sys.stdout.buffer.flush()
"#;
        let mut t = spawn_stub(script).await;
        let err = t
            .send_request(&init_request(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("invalid UTF-8 should error");
        assert!(err.to_string().contains("Failed to read"), "got: {err}");
    }

    #[tokio::test]
    async fn malformed_frame_is_a_parse_error() {
        let _guard = always_on_tracing_guard();
        let script = r#"
import sys
sys.stdin.readline()
sys.stdout.write("this is not json\n")
sys.stdout.flush()
"#;
        let mut t = spawn_stub(script).await;
        let err = t
            .send_request(&init_request(), DEFAULT_REQUEST_TIMEOUT)
            .await
            .err()
            .expect("garbage should error");
        assert!(err.to_string().contains("parse"), "got: {err}");
    }

    #[tokio::test]
    async fn non_response_frame_that_is_unparseable_errors() {
        let _guard = always_on_tracing_guard();
        // Valid JSON, no `method`, but not response-shaped either.
        let script = r#"
import sys
sys.stdin.readline()
sys.stdout.write("[1,2,3]\n")
sys.stdout.flush()
"#;
        let mut t = spawn_stub(script).await;
        assert!(
            t.send_request(&init_request(), DEFAULT_REQUEST_TIMEOUT)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn notification_is_written_without_waiting() {
        let _guard = always_on_tracing_guard();
        let mut t = spawn_stub(&stub("")).await;
        t.send_notification(&JsonRpcRequest::notification(
            "notifications/initialized",
            serde_json::json!({}),
        ))
        .await
        .expect("notification should be written");
    }

    #[tokio::test]
    async fn close_is_graceful_and_idempotent() {
        let _guard = always_on_tracing_guard();
        let mut t = spawn_stub(&stub("")).await;
        t.close().await.expect("close should succeed");
        t.close().await.expect("close should stay successful");
    }

    /// Kill and reap the child so the pipe's read end is provably gone, making
    /// the broken-pipe write/flush arms deterministic rather than racy.
    async fn spawn_and_kill() -> StdioTransport {
        let mut t = spawn_stub(&stub("")).await;
        t.child.kill().await.expect("kill should succeed");
        let _ = t.child.wait().await;
        // A tiny buffered write's flush doesn't reliably surface EPIPE the
        // instant the child is reaped; a short delay lets the kernel settle the
        // closed pipe state. (A >8KB write bypasses BufWriter and hits the OS
        // directly, so those cases are deterministic without this.)
        tokio::time::sleep(Duration::from_millis(50)).await;
        t
    }

    #[tokio::test]
    async fn notification_flush_after_child_death_errors() {
        let mut t = spawn_and_kill().await;
        let result = t
            .send_notification(&JsonRpcRequest::notification(
                "notifications/test",
                serde_json::json!({}),
            ))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn notification_write_all_after_child_death_errors() {
        let mut t = spawn_and_kill().await;
        // >8KB exceeds BufWriter's capacity, forcing write_all to hit the OS.
        let huge = "x".repeat(20_000);
        let result = t
            .send_notification(&JsonRpcRequest::notification(
                "notifications/test",
                serde_json::json!({ "data": huge }),
            ))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn request_write_after_child_death_errors() {
        let mut t = spawn_and_kill().await;
        let huge = "x".repeat(20_000);
        let result = t
            .send_request(
                &JsonRpcRequest::request(1, "tools/call", serde_json::json!({ "data": huge })),
                DEFAULT_REQUEST_TIMEOUT,
            )
            .await;
        assert!(result.is_err());
    }
}
