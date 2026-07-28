//! Sub-agent tool handlers: turn `spawn_agent` / `check_agent` /
//! `wait_for_agent` / `send_to_agent` / `kill_agent` tool calls into
//! [`SubAgentOp`]s serviced by the host (which owns the world + spawner). The
//! tool lane runs off the world, so it blocks on the host applying each op via a
//! oneshot — the same shape as an interaction.

use std::time::Duration;

use leviath_providers::ToolCall;
use leviath_runtime::components::AgentStatus;
use leviath_runtime::host::SubAgentOp;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::daemon::client::resolve_spawn_args;

/// Per-agent state needed to service the sub-agent tools: a sender into the
/// host's [`SubAgentOp`] channel plus the spawning agent's identity and the
/// context children inherit.
#[derive(Clone)]
pub struct SubAgentHandle {
    /// Sender into the host's sub-agent op channel.
    pub sender: UnboundedSender<SubAgentOp>,
    /// The run id of the agent that owns this handle (the would-be parent).
    pub parent_run_id: String,
    /// Working directory children inherit.
    pub workdir: String,
    /// Maximum allowed sub-agent tree depth.
    pub max_depth: usize,
    /// The parent run's `--no-seed-commands` setting, inherited by children so a
    /// per-run opt-out can't be side-stepped by spawning a sub-agent whose
    /// blueprint declares command seeds.
    pub no_seed_commands: bool,
}

/// The tool names routed to the sub-agent handler.
pub const SUBAGENT_TOOLS: &[&str] = &[
    "spawn_agent",
    "check_agent",
    "wait_for_agent",
    "send_to_agent",
    "kill_agent",
];

/// Whether `name` is a sub-agent tool (routed here rather than to builtin/MCP).
pub fn is_subagent_tool(name: &str) -> bool {
    SUBAGENT_TOOLS.contains(&name)
}

/// How often `wait_for_agent` / `spawn_agent(wait=true)` polls the child.
const WAIT_POLL: Duration = Duration::from_millis(500);

/// Dispatch one sub-agent tool call, returning the textual result for the model.
pub async fn handle(h: &SubAgentHandle, tc: &ToolCall) -> String {
    match tc.name.as_str() {
        "spawn_agent" => spawn(h, &tc.arguments).await,
        "check_agent" => check(h, str_arg(&tc.arguments, "agent_id")).await,
        "wait_for_agent" => wait(h, str_arg(&tc.arguments, "agent_id")).await,
        "send_to_agent" => send(h, &tc.arguments).await,
        "kill_agent" => kill(h, str_arg(&tc.arguments, "agent_id")).await,
        other => format!("[error] '{other}' is not a sub-agent tool"),
    }
}

/// Whether `blueprint`, read as a path, lands inside `workdir`.
///
/// Symlink-aware, so a link planted in the workspace cannot be used to point at
/// something that only *looks* outside it. A bare agent name is not a path that
/// exists here, so it is never caught by this.
fn resolves_within_workdir(blueprint: &str, workdir: &str) -> bool {
    let candidate = std::path::Path::new(blueprint);
    let workdir = std::path::Path::new(workdir);
    // Only an existing path can be one the agent just wrote.
    if !candidate.exists() {
        let joined = workdir.join(blueprint);
        return joined.exists() && leviath_core::resolves_within(&joined, workdir);
    }
    leviath_core::resolves_within(candidate, workdir)
}

/// A required string argument, or `""` when missing/not a string.
fn str_arg<'a>(args: &'a serde_json::Value, key: &str) -> &'a str {
    args.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

async fn spawn(h: &SubAgentHandle, args: &serde_json::Value) -> String {
    let blueprint = str_arg(args, "blueprint");
    let task = str_arg(args, "task");
    if blueprint.is_empty() || task.is_empty() {
        return "[error] spawn_agent requires 'blueprint' and 'task'".to_string();
    }
    // Never a blueprint the agent could have written itself.
    //
    // `blueprint` comes from model output and `find_manifest` accepts any path.
    // `write_file` is confined to the workdir — but the spawner was not, so a
    // model steered by injected content could write `x/agent.leviath` inside its
    // own workdir and then spawn it. The child is built with seeds enforced, so
    // that manifest's `seed = { command = ... }` ran on the host before its
    // first inference, and its `[[mcp_servers]]` spawned arbitrary programs:
    // a confined file write escalated to unconfined command execution.
    //
    // Refusing paths *inside the workdir* closes that exactly, and leaves
    // everything legitimate working — an installed agent by name, or a path a
    // human or the parent blueprint chose. A model that can already write
    // outside the workdir has arbitrary execution by other means, so nothing
    // here is the weak link.
    if resolves_within_workdir(blueprint, &h.workdir) {
        return format!(
            "[error] '{blueprint}' is inside this agent's own working directory. \
             Spawn an installed agent by name, or a blueprint from outside the \
             workspace — an agent may not author the blueprint it runs."
        );
    }

    // Optional seed context is prepended to the task (it lands in the child's
    // pinned task region, which is exactly what the parent wants seeded).
    let full_task = match args.get("seed_context").and_then(|v| v.as_str()) {
        Some(seed) if !seed.is_empty() => format!("{task}\n\nContext:\n{seed}"),
        _ => task.to_string(),
    };
    let child_max_depth = args
        .get("max_child_depth")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let wait_flag = args.get("wait").and_then(|v| v.as_bool()).unwrap_or(false);

    let spawn_args = match resolve_spawn_args(
        blueprint,
        &full_task,
        None,
        &h.workdir,
        false,
        Vec::new(),
        child_max_depth,
        // Sub-agents receive their whole task via `full_task`; no region flags.
        std::collections::HashMap::new(),
        h.no_seed_commands,
    ) {
        Ok(a) => a,
        Err(e) => return format!("[error] cannot spawn '{blueprint}': {e}"),
    };

    let (tx, rx) = oneshot::channel();
    if h.sender
        .send(SubAgentOp::Spawn {
            args: Box::new(spawn_args),
            parent_run_id: h.parent_run_id.clone(),
            max_depth: h.max_depth,
            reply: tx,
        })
        .is_err()
    {
        return "[error] the daemon is shutting down".to_string();
    }
    match rx.await {
        Ok(Ok(child_id)) if wait_flag => wait(h, &child_id).await,
        Ok(Ok(child_id)) => format!("Spawned sub-agent '{child_id}'."),
        Ok(Err(e)) => format!("[error] {e}"),
        Err(_) => "[error] the daemon dropped the spawn request".to_string(),
    }
}

async fn check(h: &SubAgentHandle, agent_id: &str) -> String {
    match status_of(h, agent_id).await {
        Some(status) => format!("Sub-agent '{agent_id}' status: {}", label(&status)),
        None => format!("[error] no such sub-agent '{agent_id}'"),
    }
}

async fn wait(h: &SubAgentHandle, agent_id: &str) -> String {
    if agent_id.is_empty() {
        return "[error] wait_for_agent requires 'agent_id'".to_string();
    }
    loop {
        match status_of(h, agent_id).await {
            None => return format!("[error] no such sub-agent '{agent_id}'"),
            Some(status) if is_terminal(&status) => {
                return format!(
                    "Sub-agent '{agent_id}' finished with status: {}",
                    label(&status)
                );
            }
            // The caller itself was cancelled (or failed) while waiting. Give up
            // rather than keep polling for a child that is being torn down with
            // it — this loop has no other exit, so it would hold its tool-lane
            // worker for as long as the daemon lived.
            Some(_) if caller_is_terminal(h).await => {
                return format!("[error] cancelled while waiting for '{agent_id}'");
            }
            Some(_) => tokio::time::sleep(WAIT_POLL).await,
        }
    }
}

/// Whether the agent that called `wait_for_agent` has itself reached a terminal
/// state. A dropped request (daemon shutting down) counts as terminal — there is
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
    let (tx, rx) = oneshot::channel();
    if h.sender
        .send(SubAgentOp::Send {
            run_id: agent_id.to_string(),
            caller_run_id: h.parent_run_id.clone(),
            content: message.to_string(),
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
async fn status_of(h: &SubAgentHandle, agent_id: &str) -> Option<AgentStatus> {
    let (tx, rx) = oneshot::channel();
    h.sender
        .send(SubAgentOp::Check {
            run_id: agent_id.to_string(),
            reply: tx,
        })
        .ok()?;
    rx.await.ok().flatten()
}

fn is_terminal(status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Complete | AgentStatus::Cancelled | AgentStatus::Error { .. }
    )
}

fn label(status: &AgentStatus) -> String {
    match status {
        AgentStatus::Idle => "idle".to_string(),
        AgentStatus::Active => "active".to_string(),
        AgentStatus::Waiting => "waiting".to_string(),
        AgentStatus::Complete => "complete".to_string(),
        AgentStatus::Cancelled => "cancelled".to_string(),
        AgentStatus::Error { message } => format!("error: {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The escalation this closes: `write_file` is confined to the workdir, but
    /// the spawner was not — so a model could author `x/agent.leviath` in its own
    /// workspace and spawn it, and the child is built with seeds enforced, so
    /// that manifest's command seeds ran on the host before its first inference.
    #[tokio::test]
    async fn spawn_refuses_a_blueprint_the_agent_could_have_written() {
        let work = tempfile::tempdir().unwrap();
        // Exactly what the model would produce: a manifest inside its workdir.
        let planted = work.path().join("x");
        std::fs::create_dir(&planted).unwrap();
        std::fs::write(planted.join("agent.leviath"), "[agent]\nname = \"x\"\n").unwrap();

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let h = SubAgentHandle {
            sender: tx,
            parent_run_id: "parent".to_string(),
            workdir: work.path().to_string_lossy().to_string(),
            max_depth: 3,
            no_seed_commands: false,
        };

        for bad in [
            planted.to_string_lossy().to_string(),
            "x".to_string(),
            "x/agent.leviath".to_string(),
        ] {
            let out = spawn(&h, &serde_json::json!({"blueprint": bad, "task": "go"})).await;
            assert!(
                out.contains("own working directory"),
                "{bad} must be refused: {out}"
            );
        }
    }

    /// And a blueprint from outside the workspace is untouched — an installed
    /// agent by name, or a path a human chose.
    #[tokio::test]
    async fn spawn_allows_a_blueprint_outside_the_workdir() {
        let work = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(
            elsewhere.path().join("agent.leviath"),
            "[agent]\nname = \"x\"\n",
        )
        .unwrap();

        // The receiver is dropped so the op fails fast rather than waiting on a
        // reply no host is here to send. What this asserts is that the path
        // check let the blueprint through, not that a spawn succeeded.
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let h = SubAgentHandle {
            sender: tx,
            parent_run_id: "parent".to_string(),
            workdir: work.path().to_string_lossy().to_string(),
            max_depth: 3,
            no_seed_commands: false,
        };
        let out = spawn(
            &h,
            &serde_json::json!({
                "blueprint": elsewhere.path().to_string_lossy(),
                "task": "go"
            }),
        )
        .await;
        assert!(
            !out.contains("own working directory"),
            "a blueprint outside the workspace must not be refused: {out}"
        );
    }
    use leviath_runtime::host::SpawnArgs;
    use serde_json::json;

    fn handle_with(sender: UnboundedSender<SubAgentOp>) -> SubAgentHandle {
        SubAgentHandle {
            sender,
            parent_run_id: "parent".to_string(),
            // This crate's own directory, deliberately *not* the system temp
            // dir: `temp_blueprint()` writes under temp, and on Linux that is
            // `/tmp` — so a workdir of `/tmp` made every fixture blueprint look
            // like one the agent had planted in its own workspace, and the
            // containment guard refused them all. macOS puts tempdirs under
            // `$TMPDIR` in `/var/folders`, so nothing local caught it.
            workdir: env!("CARGO_MANIFEST_DIR").to_string(),
            max_depth: 3,
            no_seed_commands: false,
        }
    }

    /// A `SubAgentHandle` whose host answers each op from plain canned values —
    /// no per-call-site closures, so this single service loop is the only region
    /// (covered collectively across the suite). `spawn_result` answers `Spawn`
    /// and the received args are recorded into the returned `Vec` for assertions;
    /// `statuses` answers successive `Check`s for *children* in order (`None`
    /// once exhausted); `ok` answers `Send`/`Kill`. The caller ("parent") is
    /// reported `Active` — see [`fake_host_with_parent`] to script it.
    #[allow(clippy::type_complexity)]
    fn fake_host(
        spawn_result: Result<String, String>,
        statuses: Vec<Option<AgentStatus>>,
        ok: bool,
    ) -> (
        SubAgentHandle,
        std::sync::Arc<std::sync::Mutex<Vec<SpawnArgs>>>,
        tokio::task::JoinHandle<()>,
    ) {
        fake_host_with_parent(spawn_result, statuses, ok, Some(AgentStatus::Active))
    }

    /// [`fake_host`] with the calling agent's own status scripted too.
    #[allow(clippy::type_complexity)]
    fn fake_host_with_parent(
        spawn_result: Result<String, String>,
        statuses: Vec<Option<AgentStatus>>,
        ok: bool,
        parent_status: Option<AgentStatus>,
    ) -> (
        SubAgentHandle,
        std::sync::Arc<std::sync::Mutex<Vec<SpawnArgs>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_task = seen.clone();
        let task = tokio::spawn(async move {
            let mut checks = statuses.into_iter();
            while let Some(op) = rx.recv().await {
                match op {
                    SubAgentOp::Spawn { reply, args, .. } => {
                        seen_task.lock().unwrap().push(*args);
                        let _ = reply.send(spawn_result.clone());
                    }
                    // `wait` polls the *caller* as well as the child (to bail out
                    // if the caller was itself cancelled), so the scripted queue
                    // answers only for children — the caller is reported Active
                    // unless a test scripts it otherwise.
                    SubAgentOp::Check { reply, run_id } if run_id == "parent" => {
                        let _ = reply.send(parent_status.clone());
                    }
                    SubAgentOp::Check { reply, .. } => {
                        let _ = reply.send(checks.next().flatten());
                    }
                    SubAgentOp::Send { reply, .. } => {
                        let _ = reply.send(ok);
                    }
                    SubAgentOp::Kill { reply, .. } => {
                        let _ = reply.send(ok);
                    }
                }
            }
        });
        (handle_with(tx), seen, task)
    }

    /// A host that drops every op without replying — the handler then sees a
    /// dropped oneshot.
    fn drop_host() -> (SubAgentHandle, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            while let Some(op) = rx.recv().await {
                drop(op);
            }
        });
        (handle_with(tx), task)
    }

    /// A handle whose host is already gone (sends fail immediately).
    fn dead_handle() -> SubAgentHandle {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        handle_with(tx)
    }

    /// Write a minimal valid blueprint into a temp dir and return that dir (whose
    /// path `find_manifest` resolves to `<dir>/agent.leviath`).
    fn temp_blueprint() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("agent.leviath"),
            r#"
[agent]
name = "child"
version = "0.1.0"
description = "child"

[stages.main]
model = { provider = "anthropic", model = "claude-sonnet-4-6" }
"#,
        )
        .unwrap();
        dir
    }

    fn tc(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "1".to_string(),
            name: name.to_string(),
            arguments: args,
        }
    }

    #[test]
    fn is_subagent_tool_recognizes_the_five_names() {
        for name in SUBAGENT_TOOLS {
            assert!(is_subagent_tool(name));
        }
        assert!(!is_subagent_tool("read_file"));
    }

    #[test]
    fn label_and_terminal_cover_all_statuses() {
        assert_eq!(label(&AgentStatus::Idle), "idle");
        assert_eq!(label(&AgentStatus::Active), "active");
        assert_eq!(label(&AgentStatus::Waiting), "waiting");
        assert_eq!(label(&AgentStatus::Complete), "complete");
        assert_eq!(label(&AgentStatus::Cancelled), "cancelled");
        assert_eq!(
            label(&AgentStatus::Error {
                message: "boom".to_string()
            }),
            "error: boom"
        );
        for s in [AgentStatus::Active, AgentStatus::Waiting, AgentStatus::Idle] {
            assert!(!is_terminal(&s));
        }
        for s in [
            AgentStatus::Complete,
            AgentStatus::Cancelled,
            AgentStatus::Error {
                message: "x".to_string(),
            },
        ] {
            assert!(is_terminal(&s));
        }
    }

    #[tokio::test]
    async fn spawn_resolves_blueprint_forwards_seed_and_reports_the_child_id() {
        let bp = temp_blueprint();
        let (h, seen, t) = fake_host(Ok("child-123".to_string()), vec![], false);
        let out = handle(
            &h,
            &tc(
                "spawn_agent",
                json!({
                    "blueprint": bp.path().to_str().unwrap(),
                    "task": "do it",
                    "seed_context": "prior findings",
                    "max_child_depth": 2
                }),
            ),
        )
        .await;
        assert!(out.contains("Spawned sub-agent 'child-123'"));
        // Drop the handle and drain the host task — covers the loop's exit.
        drop(h);
        t.await.unwrap();
        // The seed context was folded into the child's task.
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].task.contains("do it") && seen[0].task.contains("prior findings"));
        assert_eq!(seen[0].max_depth, Some(2));
    }

    #[tokio::test]
    async fn spawn_with_wait_blocks_until_the_child_finishes() {
        let bp = temp_blueprint();
        // Active on the first poll, Complete after.
        let (h, _seen, _t) = fake_host(
            Ok("child-1".to_string()),
            vec![Some(AgentStatus::Active), Some(AgentStatus::Complete)],
            false,
        );
        let out = handle(
            &h,
            &tc(
                "spawn_agent",
                json!({ "blueprint": bp.path().to_str().unwrap(), "task": "t", "wait": true }),
            ),
        )
        .await;
        assert!(out.contains("finished with status: complete"));
    }

    /// `wait_for_agent` gives up when the *calling* agent is cancelled. The loop
    /// has no other exit, and it runs on a tool-lane worker — of which there are
    /// a fixed number — so a cancelled caller would otherwise poll for a child
    /// that is being torn down with it until the daemon exits.
    #[tokio::test]
    async fn wait_gives_up_when_the_calling_agent_is_cancelled() {
        let (h, _seen, _t) = fake_host_with_parent(
            Ok("child-1".to_string()),
            // The child never finishes on its own.
            vec![Some(AgentStatus::Active); 8],
            false,
            Some(AgentStatus::Cancelled),
        );
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle(&h, &tc("wait_for_agent", json!({ "agent_id": "child-1" }))),
        )
        .await
        .expect("the wait returns instead of polling forever");
        assert!(
            out.contains("cancelled while waiting"),
            "reports why it stopped, got: {out}"
        );
    }

    /// A caller the host no longer knows about (daemon shutting down, or the
    /// run already reaped) also ends the wait — there is nothing left to wait
    /// for either way.
    #[tokio::test]
    async fn wait_gives_up_when_the_caller_is_unknown_to_the_host() {
        let (h, _seen, _t) = fake_host_with_parent(
            Ok("child-1".to_string()),
            vec![Some(AgentStatus::Active); 8],
            false,
            None, // the host has no such caller
        );
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            handle(&h, &tc("wait_for_agent", json!({ "agent_id": "child-1" }))),
        )
        .await
        .expect("the wait returns instead of polling forever");
        assert!(out.contains("cancelled while waiting"), "got: {out}");
    }

    #[tokio::test]
    async fn spawn_requires_blueprint_and_task_and_reports_resolve_errors() {
        let (h, _seen, _t) = fake_host(Ok(String::new()), vec![], false);
        assert!(
            handle(&h, &tc("spawn_agent", json!({ "task": "t" })))
                .await
                .contains("requires 'blueprint' and 'task'")
        );
        assert!(
            handle(
                &h,
                &tc(
                    "spawn_agent",
                    json!({ "blueprint": "/no/such/agent", "task": "t" })
                )
            )
            .await
            .contains("cannot spawn")
        );
    }

    #[tokio::test]
    async fn spawn_reports_spawner_error_and_dead_host() {
        let bp = temp_blueprint();
        let (h, _seen, _t) = fake_host(Err("bad blueprint".to_string()), vec![], false);
        assert!(
            handle(
                &h,
                &tc(
                    "spawn_agent",
                    json!({ "blueprint": bp.path().to_str().unwrap(), "task": "t" })
                )
            )
            .await
            .contains("bad blueprint")
        );
        assert!(
            handle(
                &dead_handle(),
                &tc(
                    "spawn_agent",
                    json!({ "blueprint": bp.path().to_str().unwrap(), "task": "t" })
                )
            )
            .await
            .contains("shutting down")
        );
    }

    #[tokio::test]
    async fn check_reports_status_or_missing() {
        let (h, _seen, _t) = fake_host(Ok(String::new()), vec![Some(AgentStatus::Active)], false);
        assert!(
            handle(&h, &tc("check_agent", json!({ "agent_id": "c" })))
                .await
                .contains("status: active")
        );
        let (h2, _seen2, _t2) = fake_host(Ok(String::new()), vec![], false);
        assert!(
            handle(&h2, &tc("check_agent", json!({ "agent_id": "c" })))
                .await
                .contains("no such sub-agent")
        );
        // A dead host: `status_of`'s send fails, so it returns `None` early.
        assert!(
            handle(
                &dead_handle(),
                &tc("check_agent", json!({ "agent_id": "c" }))
            )
            .await
            .contains("no such sub-agent")
        );
    }

    #[tokio::test]
    async fn wait_requires_id_and_returns_when_terminal_or_missing() {
        assert!(
            handle(&dead_handle(), &tc("wait_for_agent", json!({})))
                .await
                .contains("requires 'agent_id'")
        );
        let (h, _seen, _t) = fake_host(
            Ok(String::new()),
            vec![Some(AgentStatus::Error {
                message: "boom".to_string(),
            })],
            false,
        );
        assert!(
            handle(&h, &tc("wait_for_agent", json!({ "agent_id": "c" })))
                .await
                .contains("error: boom")
        );
        let (h2, _seen2, _t2) = fake_host(Ok(String::new()), vec![], false);
        assert!(
            handle(&h2, &tc("wait_for_agent", json!({ "agent_id": "c" })))
                .await
                .contains("no such sub-agent")
        );
    }

    #[tokio::test]
    async fn send_delivers_or_reports_failure() {
        let (h, _seen, _t) = fake_host(Ok(String::new()), vec![], true);
        assert!(
            handle(
                &h,
                &tc("send_to_agent", json!({ "agent_id": "c", "message": "hi" }))
            )
            .await
            .contains("Delivered message")
        );
        assert!(
            handle(&h, &tc("send_to_agent", json!({ "agent_id": "c" })))
                .await
                .contains("requires 'agent_id' and 'message'")
        );
        let (h2, _seen2, _t2) = fake_host(Ok(String::new()), vec![], false);
        assert!(
            handle(
                &h2,
                &tc("send_to_agent", json!({ "agent_id": "c", "message": "hi" }))
            )
            .await
            .contains("did not accept")
        );
        assert!(
            handle(
                &dead_handle(),
                &tc("send_to_agent", json!({ "agent_id": "c", "message": "hi" }))
            )
            .await
            .contains("shutting down")
        );
    }

    #[tokio::test]
    async fn kill_cancels_or_reports_missing() {
        let (h, _seen, _t) = fake_host(Ok(String::new()), vec![], true);
        assert!(
            handle(&h, &tc("kill_agent", json!({ "agent_id": "c" })))
                .await
                .contains("Killed sub-agent")
        );
        assert!(
            handle(&h, &tc("kill_agent", json!({})))
                .await
                .contains("requires 'agent_id'")
        );
        let (h2, _seen2, _t2) = fake_host(Ok(String::new()), vec![], false);
        assert!(
            handle(&h2, &tc("kill_agent", json!({ "agent_id": "c" })))
                .await
                .contains("no such sub-agent")
        );
        assert!(
            handle(
                &dead_handle(),
                &tc("kill_agent", json!({ "agent_id": "c" }))
            )
            .await
            .contains("shutting down")
        );
    }

    #[tokio::test]
    async fn handle_rejects_a_non_subagent_tool() {
        assert!(
            handle(&dead_handle(), &tc("read_file", json!({})))
                .await
                .contains("is not a sub-agent tool")
        );
    }

    #[tokio::test]
    async fn dropped_reply_paths_are_handled() {
        let (h, t) = drop_host();
        // status_of returns None on a dropped reply → "no such sub-agent".
        assert!(
            handle(&h, &tc("check_agent", json!({ "agent_id": "c" })))
                .await
                .contains("no such sub-agent")
        );
        assert!(
            handle(
                &h,
                &tc("send_to_agent", json!({ "agent_id": "c", "message": "m" }))
            )
            .await
            .contains("dropped the message")
        );
        assert!(
            handle(&h, &tc("kill_agent", json!({ "agent_id": "c" })))
                .await
                .contains("dropped the kill request")
        );
        let bp = temp_blueprint();
        assert!(
            handle(
                &h,
                &tc(
                    "spawn_agent",
                    json!({ "blueprint": bp.path().to_str().unwrap(), "task": "t" })
                )
            )
            .await
            .contains("dropped the spawn request")
        );
        drop(h);
        t.await.unwrap();
    }
}
