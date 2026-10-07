//! Shared test-only helpers for this crate's `#[cfg(test)]` code.
//!
//! The tracing subscriber comes from `leviath-testkit` (one workspace-wide
//! copy); the helpers below are CLI-specific fixtures.

pub(crate) use crate::test_fixtures::{FakeProvider, fixtures};
pub(crate) use leviath_testkit::mcp_stub::McpStub;
pub(crate) use leviath_testkit::with_tracing;

/// A value whose `Serialize` impl always returns `Err`, so tests can drive the
/// `?` error arm of the crate's `serde_json::to_string_pretty(...)?` helpers
/// (which serialize trivially-serializable structs that never fail on real input).
pub(crate) struct PoisonSerialize;

impl serde::Serialize for PoisonSerialize {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Err(serde::ser::Error::custom("PoisonSerialize always fails"))
    }
}

/// Whether `needle` appears anywhere in the run file at `path` once every
/// frame is decompressed: what a reader of the file could get out of it.
pub(crate) fn run_file_holds(path: &std::path::Path, needle: &str) -> bool {
    let bytes = std::fs::read(path).expect("the run file reads");
    let (frames, _) = leviath_runtime::runfile::codec::frames(&bytes);
    assert!(!frames.is_empty(), "a run file has frames");
    frames.iter().any(|f| {
        let plain = zstd::stream::decode_all(&bytes[f.body..f.body + f.len]).expect("a frame");
        plain.windows(needle.len()).any(|w| w == needle.as_bytes())
    })
}

/// Write an `agent.toml` blueprint into `dir` and return its path.
///
/// Consolidates the `std::fs::write(dir.join("agent.toml"), ...).unwrap()`
/// idiom repeated across the CLI command test modules. `contents` accepts
/// anything byte-like (`&str`, `String`, byte slices) so both blueprint text
/// and deliberately-malformed byte payloads route through the same helper.
pub(crate) fn write_test_agent(
    dir: impl AsRef<std::path::Path>,
    contents: impl AsRef<[u8]>,
) -> std::path::PathBuf {
    let path = dir.as_ref().join(leviath_blueprint::FILE_NAME);
    std::fs::write(&path, contents).unwrap();
    path
}

/// The smallest blueprint that runs: one stage, one region the task lands
/// in. `name` names it.
#[cfg(test)]
pub(crate) fn tiny_blueprint(name: &str) -> String {
    format!(
        r#"[blueprint]
name = "{name}"
version = "1.0.0"

[graph]
stages = [{{ name = "main", system_prompt = "work" }}]
layout = {{ total_budget_tokens = 1000, regions = [{{ name = "task", kind = "pinned", budget = 1000 }}] }}
inputs = [{{ name = "task", type = "text", required = true, binds = [{{ region = "task" }}] }}]
"#
    )
}

/// A self-contained, coder-shaped blueprint for daemon spawn/recovery/setup
/// tests. Deliberately does NOT read the shipped `agents/coder/agent.toml`:
/// those tests exercise spawn/reload *logic*, not the shipped blueprint, so they
/// must stay isolated from blueprint edits. Budgets are absolute (window-
/// independent) so a fake small-context test model can't starve the region the
/// stage system prompt is injected into. The `task` region is load-bearing:
/// every caller spawns this with a task, and a blueprint with nowhere to put one
/// is refused at spawn.
#[cfg(test)]
pub(crate) fn inline_coder_manifest() -> String {
    r#"[blueprint]
name = "coder"
version = "0.0.0"
description = "Inline test blueprint (coder-shaped); self-contained."

[graph]
entry = "analyze"
tool_permissions = { read_file = "allow", list_dir = "allow", write_file = "ask", bash = "ask" }
inputs = [{ name = "task", type = { kind = "text", multiline = true }, binds = [{ region = "task" }] }]

[graph.layout]
total_budget_tokens = 50000
regions = [
    { name = "system", kind = "pinned", budget = 8000 },
    { name = "task", kind = "pinned", budget = 2000 },
    { name = "codebase", kind = "temporary", budget = 20000 },
    { name = "conversation", kind = { kind = "sliding_window", max_items = 40 }, budget = 20000 },
]

[[graph.stages]]
name = "analyze"
description = "Understand the task"
model = { models = [{ provider = "anthropic", model = "m" }] }
tools = ["read_file", "list_dir"]
system_prompt = "Analyze the task and outline a short plan."

[[graph.edges]]
name = "implement"
from = "analyze"
to = "implement"

[[graph.stages]]
name = "implement"
description = "Write the code"
model = { models = [{ provider = "anthropic", model = "m" }] }
tools = ["write_file", "read_file", "list_dir", "bash"]
system_prompt = "Implement the plan."

[[graph.edges]]
name = "review"
from = "implement"
to = "review"
carry = { compact = {} }

[[graph.stages]]
name = "review"
description = "Review the code"
model = { models = [{ provider = "anthropic", model = "m" }] }
tools = ["read_file", "list_dir"]
system_prompt = "Review the implementation."
"#
    .to_string()
}

/// A fake daemon that speaks the real control handshake - a token, and the
/// identity it introduces itself with - and answers every request `respond`
/// gives it, for as long as `connections` says. Subscribers get the events
/// sent through the returned broadcast sender, until it is dropped.
///
/// The token is created in `dir` (where `ControlClient::for_home` reads it),
/// so `for_home(id, dir)` reaches it. Tests that need the daemon to *say who
/// it is* - the reconnect and update surfaces in `serve`, `dash`, and the ACP
/// bridge - use this; tests that only need answers use the tokenless
/// hand-rolled fakes beside them, which never handshake.
///
/// Deliberately trusting: the first line on every connection is taken to be
/// the handshake and answered with `Welcome` unread, and every later line is
/// taken to be a well-formed request. Only clients this crate builds talk to
/// it, and the runtime's own tests cover a daemon that refuses.
pub(crate) struct IdentifiedDaemon {
    /// A client that reaches it (with the token, and this process's build).
    pub(crate) client: leviath_runtime::control_socket::ControlClient,
    /// The accept loop, done once `connections` have been served.
    pub(crate) server: tokio::task::JoinHandle<()>,
    /// The socket's directory, kept alive as long as the daemon. Tests that
    /// need to feed subscribers events, or restart the daemon in the same
    /// directory, use [`identified_daemon_in`] directly.
    _dir: tempfile::TempDir,
}

/// The build a test daemon on "the same code" as the test's clients reports.
pub(crate) const TEST_BUILD: &str = "test-build";

/// Start an [`IdentifiedDaemon`] introducing itself as `identity`, in a fresh
/// temp dir. See [`identified_daemon_in`] to reuse a dir across restarts.
pub(crate) fn identified_daemon(
    identity: leviath_runtime::control_socket::DaemonIdentity,
    connections: usize,
    respond: impl Fn(
        leviath_runtime::control_socket::ControlRequest,
    ) -> leviath_runtime::control_socket::ControlResponse
    + Send
    + Sync
    + 'static,
) -> IdentifiedDaemon {
    let dir = tempfile::tempdir().unwrap();
    let (client, _events, server) =
        identified_daemon_in(dir.path(), identity, connections, respond);
    IdentifiedDaemon {
        client,
        server,
        _dir: dir,
    }
}

/// Start an identity-speaking fake daemon on the socket under `dir`, minting a
/// fresh token there (as a real daemon does on every start). Returns a client
/// pointed at it, the event sender, and the accept task.
pub(crate) fn identified_daemon_in(
    dir: &std::path::Path,
    identity: leviath_runtime::control_socket::DaemonIdentity,
    connections: usize,
    respond: impl Fn(
        leviath_runtime::control_socket::ControlRequest,
    ) -> leviath_runtime::control_socket::ControlResponse
    + Send
    + Sync
    + 'static,
) -> (
    leviath_runtime::control_socket::ControlClient,
    tokio::sync::broadcast::Sender<leviath_runtime::host::WorldEvent>,
    tokio::task::JoinHandle<()>,
) {
    use leviath_runtime::control_socket::{
        ControlClient, ControlRequest, ControlResponse, ControlToken, bind_control_listener,
        control_id,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let id = control_id(dir);
    let mut listener = bind_control_listener(&id).unwrap();
    let _token = ControlToken::create(dir).unwrap();
    let (events, _keep) = tokio::sync::broadcast::channel(64);
    let respond = std::sync::Arc::new(respond);
    let server_events = events.clone();
    let server = tokio::spawn(async move {
        // Joined before the task ends: a test that "restarts" the daemon
        // rebinds the same id, and on Windows a pipe instance still held by
        // a connection task makes that bind fail with `AddrInUse`.
        let mut served = Vec::new();
        for _ in 0..connections {
            let stream = listener
                .accept()
                .await
                .expect("accept succeeds")
                .expect("our own connection is admitted");
            let identity = identity.clone();
            let respond = respond.clone();
            let mut rx = server_events.subscribe();
            served.push(tokio::spawn(async move {
                let (read_half, mut write_half) = tokio::io::split(stream);
                let mut lines = BufReader::new(read_half).lines();
                let _hello = lines.next_line().await.unwrap().unwrap();
                let mut out =
                    serde_json::to_string(&ControlResponse::Welcome { daemon: identity }).unwrap();
                out.push('\n');
                let _ = write_half.write_all(out.as_bytes()).await;
                // Then requests, until the client hangs up.
                while let Ok(Some(line)) = lines.next_line().await {
                    let req = serde_json::from_str::<ControlRequest>(&line).unwrap();
                    if let ControlRequest::Subscribe = req {
                        // Until the sender is dropped, which is how a test
                        // "stops" this daemon.
                        while let Ok(event) = rx.recv().await {
                            let mut out = serde_json::to_string(&event).unwrap();
                            out.push('\n');
                            let _ = write_half.write_all(out.as_bytes()).await;
                        }
                        return;
                    }
                    let mut out = serde_json::to_string(&respond(req)).unwrap();
                    out.push('\n');
                    let _ = write_half.write_all(out.as_bytes()).await;
                }
            }));
        }
        // The listener goes now, and so does this task's hold on the event
        // sender: a subscribed connection ends only when every sender is gone,
        // and the test's own clone is the one that should decide that.
        drop(listener);
        drop(server_events);
        for connection in served {
            connection.await.unwrap();
        }
    });
    let client = ControlClient::for_home(id, dir).with_build(TEST_BUILD);
    (client, events, server)
}

/// The identity of a fake daemon on the same code as the test's clients, at
/// `pid`. Change `build` or `version` to make it "an update".
pub(crate) fn same_code_daemon(pid: u32) -> leviath_runtime::control_socket::DaemonIdentity {
    leviath_runtime::control_socket::DaemonIdentity {
        pid,
        ..leviath_runtime::control_socket::DaemonIdentity::this_process(TEST_BUILD)
    }
}

/// The control-socket line a daemon answers a refused spawn with: one issue
/// carrying `message`.
pub(crate) fn rejected_reply(message: &str) -> &'static str {
    use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpecPath};
    let issues = SpawnIssue::new(SpecPath::root(), IssueCode::Invalid, message).into();
    let line = serde_json::to_string(
        &leviath_runtime::control_socket::ControlResponse::Rejected { issues },
    )
    .expect("a response serializes");
    Box::leak(line.into_boxed_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_test_agent_creates_manifest_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_agent(dir.path(), "version = \"1.0\"\n");
        assert!(path.exists());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "version = \"1.0\"\n"
        );
    }

    #[test]
    fn poison_serialize_always_errs() {
        let err = serde_json::to_string(&PoisonSerialize).unwrap_err();
        assert!(err.to_string().contains("PoisonSerialize always fails"));
    }
}
