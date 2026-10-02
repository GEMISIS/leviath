//! An agent that starts a child the way a model would: it reads what an
//! installed blueprint takes, gets a spawn wrong, reads the refusal, fixes it,
//! starts the child, and reads the child's history.
//!
//! The same in-process host the daemon runs (see `agent_end_to_end.rs` for why
//! in-process): the real tool lane, the real sub-agent handlers, the real host
//! ops and the real resolver. Only the provider is scripted. It decides each
//! turn from the tool results already in the request, never from a counter,
//! because title generation issues inferences of its own.

use std::sync::{Arc, Mutex};

use tokio::runtime::Handle;
use tokio::sync::oneshot;

use leviath_providers::{
    ContentBlock, FinishReason, InferenceRequest, InferenceResponse, MessageContent,
    ModelCapabilities, Provider, TokenUsage, ToolCall,
};
use leviath_runtime::host::ControlOp;
use leviath_runtime::spec::inputs::RawInput;
use leviath_runtime::spec::names::BlueprintPath;
use leviath_runtime::spec::request::{SpawnRequest, SpawnSource};
use leviath_runtime::{AgentStatus, ProviderRegistry};

/// The parent's model: one tool call a turn, chosen by how many results it
/// has seen, and the results kept for the test to read once it is done.
struct Planner {
    /// Every tool result the parent saw, as of its last turn.
    seen: Arc<Mutex<Vec<String>>>,
}

impl Planner {
    fn results(request: &InferenceRequest) -> Vec<String> {
        request
            .messages
            .iter()
            .filter_map(|m| match &m.content {
                MessageContent::Blocks(blocks) => Some(blocks),
                MessageContent::Text(_) => None,
            })
            .flatten()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    fn is_planner(request: &InferenceRequest) -> bool {
        request.system.iter().any(|b| b.text.contains("You plan."))
    }

    /// The child's id, out of `spawn_agent`'s "Spawned sub-agent '<id>'.".
    fn child_id(spawned: &str) -> String {
        spawned.split('\'').nth(1).unwrap_or("no-child").to_string()
    }

    fn call(name: &str, arguments: serde_json::Value) -> Vec<ToolCall> {
        vec![ToolCall {
            id: format!("call-{name}"),
            name: name.to_string(),
            arguments,
            thought_signature: None,
        }]
    }

    fn turn(&self, request: &InferenceRequest) -> Vec<ToolCall> {
        if !Self::is_planner(request) {
            return Vec::new();
        }
        let results = Self::results(request);
        *self.seen.lock().unwrap() = results.clone();
        match results.len() {
            0 => Self::call(
                "describe_blueprint",
                serde_json::json!({"blueprint": "worker"}),
            ),
            // The first try names an input the worker does not declare.
            1 => Self::call(
                "validate_spawn",
                serde_json::json!({
                    "source": {"blueprint": "worker"},
                    "inputs": {"tsk": "say hi"}
                }),
            ),
            // The refusal named the input it knows; the fix uses it.
            2 => Self::call(
                "spawn_agent",
                serde_json::json!({
                    "source": {"blueprint": "worker"},
                    "inputs": {"task": "say hi"}
                }),
            ),
            3 => Self::call(
                "run_history",
                serde_json::json!({"run_id": Self::child_id(&results[2])}),
            ),
            _ => Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl Provider for Planner {
    async fn infer(
        &self,
        request: &InferenceRequest,
    ) -> leviath_providers::Result<InferenceResponse> {
        let tool_calls = self.turn(request);
        Ok(InferenceResponse {
            content: match tool_calls.is_empty() {
                true => "done".to_string(),
                false => String::new(),
            },
            tool_calls,
            tokens_used: TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                cached_tokens: 0,
                cache_write_tokens: 0,
                total_tokens: 2,
                reported_cost_usd: None,
            },
            finish_reason: FinishReason::Stop,
            reasoning: None,
            parts: Vec::new(),
        })
    }

    async fn count_tokens(&self, _text: &str, _model: &str) -> usize {
        1
    }

    fn max_context_tokens(&self, _model: &str) -> usize {
        100_000
    }

    fn name(&self) -> &str {
        "e2e"
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// A one-stage blueprint `name`, whose system prompt is `prompt` and whose
/// stage is given `tools`.
fn manifest(name: &str, prompt: &str, tools: &[&str]) -> String {
    let tools: Vec<String> = tools.iter().map(|t| format!("\"{t}\"")).collect();
    format!(
        r#"[blueprint]
name = "{name}"
version = "0.0.0"
description = "{prompt}"

[graph]
entry = "work"
inputs = [{{ name = "task", type = "text", binds = [{{ region = "task" }}] }}]
layout = {{ total_budget_tokens = 20500, regions = [
    {{ name = "task", kind = "pinned", budget = 500 }},
    {{ name = "conversation", kind = {{ kind = "sliding_window", max_items = 40 }}, budget = 20000 }},
] }}

[[graph.stages]]
name = "work"
model = {{ models = [{{ provider = "e2e", model = "m" }}] }}
tools = [{tools}]
system_prompt = "{prompt}"
"#,
        tools = tools.join(", ")
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_reads_validates_fixes_spawns_and_reads_its_childs_history() {
    let home = tempfile::tempdir().expect("home");
    let worker = home.path().join(".leviath").join("agents").join("worker");
    std::fs::create_dir_all(&worker).expect("agents dir");
    std::fs::write(
        worker.join("agent.toml"),
        manifest("worker", "You work.", &[]),
    )
    .expect("worker manifest");
    let planner = tempfile::tempdir().expect("planner dir");
    std::fs::write(
        planner.path().join("agent.toml"),
        manifest(
            "planner",
            "You plan.",
            &[
                "describe_blueprint",
                "validate_spawn",
                "spawn_agent",
                "run_history",
            ],
        ),
    )
    .expect("planner manifest");
    let workdir = tempfile::tempdir().expect("workdir");
    let runs = tempfile::tempdir().expect("runs dir");
    let seen = Arc::new(Mutex::new(Vec::new()));

    let vars = [
        ("LEVIATH_HOME", Some(home.path().as_os_str().to_owned())),
        ("LEVIATH_SKIP_DOTENV", Some("1".into())),
    ];
    let (parent_id, statuses) = temp_env::async_with_vars(vars, async {
        let mut providers = ProviderRegistry::new();
        providers.register("e2e".to_string(), Arc::new(Planner { seen: seen.clone() }));
        let mcp = Arc::new(tokio::sync::Mutex::new(leviath_mcp::ToolExecutor::new()));
        let mut host =
            leviath_cli::daemon::setup::build_host(leviath_cli::daemon::setup::HostParts {
                config: leviath_cli::config::Config::default(),
                providers,
                runs_dir: runs.path().to_path_buf(),
                shared_mcp: mcp,
                mcp_tool_defs: vec![],
                mcp_tool_owners: Default::default(),
                mcp_pool: leviath_cli::daemon::mcp_pool::McpPool::for_daemon(
                    Arc::new(tokio::sync::Mutex::new(leviath_mcp::ToolExecutor::new())),
                    &[],
                ),
                runtime: Handle::current(),
                now_secs: || 1_700_000_000,
                reloader: None,
                provider_reload: None,
            });
        let request = SpawnRequest {
            workdir: Some(workdir.path().to_path_buf()),
            ..SpawnRequest::new(SpawnSource::BlueprintFile(
                BlueprintPath::new(planner.path().to_string_lossy()).expect("absolute"),
            ))
        }
        .input("task", RawInput::Text("start a worker".to_string()));
        // The daemon's own loop: the parent's sub-agent tool calls reach the
        // host as ops on it, the same as in production.
        let (control, control_rx) = tokio::sync::mpsc::unbounded_channel();
        let serving = tokio::spawn(async move { host.serve(control_rx).await });
        let (reply, spawned) = oneshot::channel();
        control
            .send(ControlOp::Spawn {
                request: Box::new(request),
                reply,
            })
            .expect("the host is serving");
        let parent_id = spawned
            .await
            .expect("spawn replied")
            .expect("the parent starts")
            .run_id
            .to_string();

        // Bounded: the whole exchange is a handful of scripted turns.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut statuses = Vec::new();
        while tokio::time::Instant::now() < deadline {
            let child = seen
                .lock()
                .unwrap()
                .get(2)
                .map(|s| Planner::child_id(s))
                .unwrap_or_default();
            statuses = Vec::new();
            for run_id in [parent_id.clone(), child] {
                let (reply, status) = oneshot::channel();
                control
                    .send(ControlOp::Status { run_id, reply })
                    .expect("the host is serving");
                statuses.push(status.await.expect("status replied"));
            }
            if statuses
                .iter()
                .all(|s| matches!(s, Some(AgentStatus::Complete)))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        drop(control);
        serving.await.expect("the host stops cleanly");
        (parent_id, statuses)
    })
    .await;

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 4, "every tool ran: {seen:#?}");
    let described: serde_json::Value = serde_json::from_str(&seen[0]).expect(&seen[0]);
    assert_eq!(described["inputs"][0]["name"], "task", "{described}");
    assert_eq!(described["spawn_with"]["source"]["blueprint"], "worker");

    assert!(
        seen[1].starts_with("[error] validate_spawn refused: 1 problem."),
        "{}",
        seen[1]
    );
    assert!(seen[1].contains("1. inputs.tsk: unknown"), "{}", seen[1]);
    assert!(seen[1].contains("Known: task"), "{}", seen[1]);

    assert!(seen[2].starts_with("Spawned sub-agent '"), "{}", seen[2]);
    let child = Planner::child_id(&seen[2]);

    let history: serde_json::Value = serde_json::from_str(&seen[3]).expect(&seen[3]);
    assert_eq!(history["run_id"], child);
    assert_eq!(history["started_as"]["title"], "worker");
    assert_eq!(history["stage"], "work");

    assert!(
        matches!(statuses[0], Some(AgentStatus::Complete)),
        "parent {parent_id}: {:?}",
        statuses[0]
    );
    assert!(
        matches!(statuses[1], Some(AgentStatus::Complete)),
        "child {child}: {:?}",
        statuses[1]
    );
}
