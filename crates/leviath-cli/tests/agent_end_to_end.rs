//! One real agent, driven through one real tool call, by the real host.
//!
//! Everything below this file is production: `build_host` builds the same
//! `WorldHost` the daemon runs, the same `ControlOp::Spawn` path loads the
//! blueprint and registers per-agent tool state, and the same pipeline systems
//! dispatch the inference, execute the tool, and resolve the transition. Only
//! two things are substituted, and both are the outside world: the provider,
//! and the clock.
//!
//! # Why this file exists
//!
//! The unit tests cover each of those systems, and they were all green while
//! several bugs shipped that only a whole run could show: a fan-out reporting
//! ten empty sections as successes, a stage advertising no tools so a policy
//! test passed vacuously, an unanswered checkpoint approved on timeout. Each was
//! found by driving a live daemon by hand, and each time that evidence
//! evaporated with the terminal session. This is the cheapest permanent form of
//! it.
//!
//! # Why in-process rather than a spawned daemon
//!
//! A spawned binary would add a socket, a process, and a poll loop, and the poll
//! loop is where flake lives. `run_until_idle` is wake-driven and bounded, so
//! this test has no sleeps, no timeouts, and no polling: it either reaches a
//! fixed point or it fails. What a spawned daemon would additionally prove is
//! the socket transport, which `control_socket` already tests directly.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::runtime::Handle;
use tokio::sync::{Mutex, oneshot};

use leviath_providers::{
    ContentBlock, FinishReason, InferenceRequest, InferenceResponse, MessageContent,
    ModelCapabilities, Provider, TokenUsage, ToolCall,
};
use leviath_runtime::host::ControlOp;
use leviath_runtime::spec::inputs::RawInput;
use leviath_runtime::spec::launch::LaunchRequest;
use leviath_runtime::spec::names::{BlueprintPath, ToolName};
use leviath_runtime::spec::request::{SpawnRequest, SpawnSource};
use leviath_runtime::{AgentStatus, ProviderRegistry};

/// A model that asks to write one file, then answers.
///
/// **Stateless on purpose.** It decides from what the request already contains
/// rather than counting turns. A turn counter is the obvious way to write this
/// and it is wrong here: title generation issues its own inference, and a retry
/// issues another, so a counter quietly attributes the tool turn to the wrong
/// call and the test passes while proving nothing. Asking "have I already been
/// handed a tool result?" cannot drift that way.
struct WritesThenAnswers {
    // No target field, deliberately: handing the provider the filename would
    // let this test pass with the task text dropped somewhere between `--task`
    // and the prompt, which is a bug this repo has actually shipped. The target
    // is read back out of the prompt instead, so the file on disk is evidence
    // that the task arrived.
    /// How many times the model was consulted *after* the tool had run, so the
    /// test can prove the result made it back into context.
    ///
    /// Counting total calls instead would be wrong, and measurably so: this run
    /// issues **three** inferences, not two, because one-shot title generation
    /// is its own call. That is the same trap the struct doc describes, and it
    /// caught this test on the first run.
    answering_turns: Arc<AtomicUsize>,
}

impl WritesThenAnswers {
    /// The word after `write ` in whatever the model was shown.
    ///
    /// Scans the system blocks as well as the messages, because which of the
    /// two a pinned region is rendered into is the context assembler's business
    /// and not something this test should pin.
    fn target_from_prompt(request: &InferenceRequest) -> Option<String> {
        let from_system = request.system.iter().map(|b| b.text.clone());
        let from_messages = request.messages.iter().filter_map(|m| match &m.content {
            MessageContent::Text(t) => Some(t.clone()),
            MessageContent::Blocks(_) => None,
        });
        from_system.chain(from_messages).find_map(|text| {
            text.split_whitespace()
                .skip_while(|w| !w.eq_ignore_ascii_case("write"))
                .nth(1)
                .map(str::to_string)
        })
    }

    fn already_ran_the_tool(request: &InferenceRequest) -> bool {
        request.messages.iter().any(|m| match &m.content {
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. })),
            MessageContent::Text(_) => false,
        })
    }
}

#[async_trait::async_trait]
impl Provider for WritesThenAnswers {
    async fn infer(
        &self,
        request: &InferenceRequest,
    ) -> leviath_providers::Result<InferenceResponse> {
        let tool_calls = match Self::already_ran_the_tool(request) {
            true => {
                self.answering_turns.fetch_add(1, Ordering::SeqCst);
                Vec::new()
            }
            false => vec![ToolCall {
                id: "call-1".to_string(),
                name: "write_file".to_string(),
                arguments: serde_json::json!({
                    "path": Self::target_from_prompt(request)
                        .unwrap_or_else(|| "the-task-never-arrived".to_string()),
                    "content": "written by the agent\n",
                }),
                thought_signature: None,
            }],
        };
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

/// A one-stage agent whose only job is to write a file and stop.
///
/// Deliberately minimal: a multi-stage blueprint would make a failure ambiguous
/// between "the tool never ran" and "the transition never resolved".
fn one_stage_manifest() -> &'static str {
    r#"[blueprint]
name = "e2e"
version = "0.0.0"
description = "Writes one file, then finishes."

[graph]
entry = "work"

[[graph.stages]]
name = "work"
description = "Write the file"
system_prompt = "Write the file you were asked for, then stop."
model = { models = [{ provider = "e2e", model = "m" }] }
tools = ["write_file"]

[graph.layout]
total_budget_tokens = 10500

[[graph.layout.regions]]
name = "task"
kind = "pinned"
budget = 500

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 20 }
budget = 10000

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
binds = [{ region = "task" }]
"#
}

/// The whole chain, asserted on its effect rather than on its status alone.
///
/// A status of `Complete` on its own would not distinguish a run that wrote the
/// file from one that skipped the tool and answered immediately, which is
/// exactly the shape of the empty-output bugs this repo has shipped. So the file
/// on disk is the primary assertion and the status is the secondary one.
#[tokio::test]
async fn an_agent_runs_a_tool_and_the_file_lands_on_disk() {
    let agent_dir = tempfile::tempdir().expect("agent dir");
    let manifest = agent_dir.path().join("agent.toml");
    std::fs::write(&manifest, one_stage_manifest()).expect("write manifest");

    let workdir = tempfile::tempdir().expect("workdir");
    let runs = tempfile::tempdir().expect("runs dir");

    let answering_turns = Arc::new(AtomicUsize::new(0));
    let mut providers = ProviderRegistry::new();
    providers.register(
        "e2e".to_string(),
        Arc::new(WritesThenAnswers {
            answering_turns: answering_turns.clone(),
        }),
    );

    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let mut host = leviath_cli::daemon::setup::build_host(leviath_cli::daemon::setup::HostParts {
        config: leviath_cli::config::Config::default(),
        providers,
        runs_dir: runs.path().to_path_buf(),
        shared_mcp: mcp,
        mcp_tool_defs: vec![],
        mcp_tool_owners: Default::default(),
        mcp_pool: leviath_cli::daemon::mcp_pool::McpPool::for_daemon(
            Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            &[],
        ),
        runtime: Handle::current(),
        // A fixed clock, so nothing here is a function of how long CI took.
        now_secs: || 1_700_000_000,
        reloader: None,
        provider_reload: None,
    });

    let blueprint =
        BlueprintPath::new(agent_dir.path().to_string_lossy()).expect("an absolute directory");
    let request = SpawnRequest {
        workdir: Some(workdir.path().to_path_buf()),
        // The narrow launch override rather than unattended: this grants
        // exactly the one tool under test, so an approval prompt appearing for
        // anything else still fails the run instead of being waved through.
        launch: LaunchRequest {
            allow: vec![ToolName::new("write_file").expect("a tool name")],
            ..LaunchRequest::default()
        },
        ..SpawnRequest::new(SpawnSource::BlueprintFile(blueprint))
    }
    .input("task", RawInput::Text("write output.txt".to_string()));
    let (reply, spawned) = oneshot::channel();
    host.handle(ControlOp::Spawn {
        request: Box::new(request),
        reply,
    });
    host.finish_starts().await;
    let run_id = spawned
        .await
        .expect("spawn replied")
        .expect("the run starts")
        .to_string();

    // Wake-driven and bounded: no sleeps, no polling, no wall-clock margin.
    host.world_mut().run_until_idle(64).await;

    // The effect, first. This is the assertion that a status alone cannot make.
    let written = workdir.path().join("output.txt");
    assert!(
        written.exists(),
        "the agent's write_file never reached the filesystem"
    );
    assert_eq!(
        std::fs::read_to_string(&written).expect("read the written file"),
        "written by the agent\n"
    );

    // The model was consulted again *after* the tool ran, which is what proves
    // the result was routed back into the context window rather than dropped.
    // Asserted as "at least one" rather than an exact total on purpose - see
    // `WritesThenAnswers`, where an exact count is a trap.
    assert!(
        answering_turns.load(Ordering::SeqCst) >= 1,
        "the tool result never came back to the model"
    );

    let (reply, status) = oneshot::channel();
    host.handle(ControlOp::Status { run_id, reply });
    let status = status.await.expect("status replied");
    assert!(
        matches!(status, Some(AgentStatus::Complete)),
        "run did not finish cleanly: {status:?}"
    );
}

/// A model that answers every turn with the same word and calls no tools.
struct AlwaysDone;

#[async_trait::async_trait]
impl Provider for AlwaysDone {
    async fn infer(
        &self,
        _request: &InferenceRequest,
    ) -> leviath_providers::Result<InferenceResponse> {
        Ok(InferenceResponse {
            content: "done".to_string(),
            tool_calls: Vec::new(),
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

/// Two stages joined by one edge the run always takes.
fn two_stage_manifest() -> &'static str {
    r#"[blueprint]
name = "e2e-two"
version = "0.0.0"
description = "Plans, then builds."

[graph]
entry = "plan"

[[graph.stages]]
name = "plan"
description = "Plan"
system_prompt = "Say done."
model = { models = [{ provider = "e2e", model = "m" }] }

[[graph.stages]]
name = "build"
description = "Build"
system_prompt = "Say done."
model = { models = [{ provider = "e2e", model = "m" }] }

[[graph.edges]]
name = "start-building"
from = "plan"
to = "build"

[graph.layout]
total_budget_tokens = 10500

[[graph.layout.regions]]
name = "task"
kind = "pinned"
budget = 500

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 20 }
budget = 10000

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
binds = [{ region = "task" }]
"#
}

/// A host over `runs`, answering every inference from [`AlwaysDone`].
fn always_done_host(runs: &std::path::Path) -> leviath_runtime::host::WorldHost {
    let mut providers = ProviderRegistry::new();
    providers.register("e2e".to_string(), Arc::new(AlwaysDone));
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    leviath_cli::daemon::setup::build_host(leviath_cli::daemon::setup::HostParts {
        config: leviath_cli::config::Config::default(),
        providers,
        runs_dir: runs.to_path_buf(),
        shared_mcp: mcp,
        mcp_tool_defs: vec![],
        mcp_tool_owners: Default::default(),
        mcp_pool: leviath_cli::daemon::mcp_pool::McpPool::for_daemon(
            Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            &[],
        ),
        runtime: Handle::current(),
        now_secs: || 1_700_000_000,
        reloader: None,
        provider_reload: None,
    })
}

/// A run that takes an edge records the move in its run file, so every
/// reader that counts edges from the file (the REST and GraphQL graphs, run
/// history) sees it taken once.
#[tokio::test]
async fn a_run_that_takes_an_edge_records_the_move_in_its_run_file() {
    let agent_dir = tempfile::tempdir().expect("agent dir");
    std::fs::write(agent_dir.path().join("agent.toml"), two_stage_manifest())
        .expect("write manifest");
    let workdir = tempfile::tempdir().expect("workdir");
    let runs = tempfile::tempdir().expect("runs dir");
    let mut host = always_done_host(runs.path());
    let blueprint =
        BlueprintPath::new(agent_dir.path().to_string_lossy()).expect("an absolute directory");
    let request = SpawnRequest {
        workdir: Some(workdir.path().to_path_buf()),
        ..SpawnRequest::new(SpawnSource::BlueprintFile(blueprint))
    }
    .input("task", RawInput::Text("plan then build".to_string()));
    let (reply, spawned) = oneshot::channel();
    host.handle(ControlOp::Spawn {
        request: Box::new(request),
        reply,
    });
    host.finish_starts().await;
    let run_id = spawned
        .await
        .expect("spawn replied")
        .expect("the run starts")
        .to_string();
    host.world_mut().run_until_idle(64).await;
    // A closed control channel ends the serve loop, which writes every
    // queued step to the run file before it returns.
    let (control, control_rx) = tokio::sync::mpsc::unbounded_channel();
    drop(control);
    host.serve(control_rx).await;

    let file = leviath_runtime::runfile::RunFileReader::open(
        &runs
            .path()
            .join(&run_id)
            .join(leviath_core::files::RUN_FILE),
    )
    .expect("the run file");
    let moves: Vec<_> = file
        .deltas(1, file.last_seq())
        .expect("every step reads")
        .into_iter()
        .flat_map(|delta| delta.changes)
        .filter_map(|change| match change {
            leviath_runtime::state::Change::LastTransition(Some(record)) => Some(record),
            _ => None,
        })
        .collect();
    assert_eq!(moves.len(), 1, "one move recorded: {moves:?}");
    assert_eq!(moves[0].from.as_str(), "plan");
    assert_eq!(moves[0].to.as_str(), "build");
    assert_eq!(
        moves[0].edge.as_ref().map(|e| e.as_str()),
        Some("start-building")
    );
    let last = file.latest_state().expect("the last state");
    assert_eq!(last.last_transition.as_ref(), Some(&moves[0]));
}
