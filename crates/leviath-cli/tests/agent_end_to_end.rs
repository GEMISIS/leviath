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
use leviath_runtime::host::{ControlOp, SpawnArgs};
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
    /// Workdir-relative path the model asks to create.
    target: String,
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
                    "path": self.target,
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
            },
            finish_reason: FinishReason::Stop,
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
    r#"[agent]
name = "e2e"
version = "0.0.0"
description = "Writes one file, then finishes."
entry_stage = "work"

[stages.work]
mode = "autonomous"
model = { provider = "e2e", model = "m" }
description = "Write the file"
available_tools = ["write_file"]
system_prompt = "Write the file you were asked for, then stop."
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
    let manifest = agent_dir.path().join("agent.leviath");
    std::fs::write(&manifest, one_stage_manifest()).expect("write manifest");

    let workdir = tempfile::tempdir().expect("workdir");
    let runs = tempfile::tempdir().expect("runs dir");

    let answering_turns = Arc::new(AtomicUsize::new(0));
    let mut providers = ProviderRegistry::new();
    providers.register(
        "e2e".to_string(),
        Arc::new(WritesThenAnswers {
            target: "output.txt".to_string(),
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
        mcp_pool: leviath_cli::daemon::mcp_pool::McpPool::for_daemon(
            Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            &[],
        ),
        runtime: Handle::current(),
        // A fixed clock, so nothing here is a function of how long CI took.
        now_secs: || 1_700_000_000,
    });

    let (reply, spawned) = oneshot::channel();
    host.handle(ControlOp::Spawn {
        args: Box::new(SpawnArgs {
            run_id: "e2e-1".to_string(),
            blueprint_path: manifest.to_string_lossy().to_string(),
            task: "write output.txt".to_string(),
            workdir: workdir.path().to_string_lossy().to_string(),
            // The narrow launch override rather than `yolo`: this grants exactly
            // the one tool under test, so an approval prompt appearing for
            // anything else still fails the run instead of being waved through.
            allow: vec!["write_file".to_string()],
            ..Default::default()
        }),
        reply,
    });
    assert_eq!(
        spawned.await.expect("spawn replied"),
        Ok("e2e-1".to_string())
    );

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
    host.handle(ControlOp::Status {
        run_id: "e2e-1".to_string(),
        reply,
    });
    let status = status.await.expect("status replied");
    assert!(
        matches!(status, Some(AgentStatus::Complete)),
        "run did not finish cleanly: {status:?}"
    );
}
