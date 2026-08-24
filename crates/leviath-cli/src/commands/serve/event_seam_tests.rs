//! One real run, from the host's change-detection pass to a websocket frame.
//!
//! # Why this file exists
//!
//! Every link in the event chain had a test, and none of them joined. A real
//! `WorldHost` emitting world events was covered in `host_tests.rs`; a *fake*
//! daemon feeding `polling.rs` was covered there; a hand-constructed
//! `ServerEvent` reaching a real websocket was covered in `websocket.rs`. So a
//! break at any seam between them - a variant the gateway forgot to map, a
//! change-detection pass that stopped running, an event the control socket
//! dropped - would pass the whole suite. Issue #502 was filed claiming exactly
//! that had happened. It had not, and nothing in the tree could say so.
//!
//! This drives the production chain end to end: `build_host` builds the same
//! host the daemon runs, `handle_connection_as` serves the same control socket,
//! `polling::event_loop` is the same relay, `ws_global` is the same route. Only
//! the provider is substituted, and that is the outside world.

use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use leviath_providers::{
    FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, Provider, TokenUsage,
};
use leviath_runtime::ProviderRegistry;
use leviath_runtime::control_socket::{
    ControlClient, ControlResponse, ControlToken, bind_control_listener, control_id,
    handle_connection_as,
};
use leviath_runtime::host::{SpawnArgs, WorldEvent};
use tokio::sync::{Mutex, broadcast};

use super::testutil::WsTestClient;
use super::types::AppState;
use crate::config::Config;

/// A model that answers in one turn and calls no tools.
///
/// Stateless, and deliberately so: the run issues more than one inference (a
/// one-shot title generation is its own call), so anything that counts turns
/// attributes them to the wrong request and the test passes while proving
/// nothing.
struct AnswersOnce;

#[async_trait::async_trait]
impl Provider for AnswersOnce {
    async fn infer(
        &self,
        _request: &InferenceRequest,
    ) -> leviath_providers::Result<InferenceResponse> {
        Ok(InferenceResponse {
            content: "done".to_string(),
            tool_calls: Vec::new(),
            // Non-zero on purpose: a `tokens` frame only fires when the totals
            // move, so a provider reporting nothing would make that frame's
            // absence indistinguishable from the bug under test.
            tokens_used: TokenUsage {
                prompt_tokens: 7,
                completion_tokens: 3,
                cached_tokens: 0,
                cache_write_tokens: 0,
                total_tokens: 10,
                reported_cost_usd: None,
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
        "seam"
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// A one-stage agent that answers and stops.
fn one_stage_manifest() -> &'static str {
    r#"[agent]
name = "seam"
version = "0.0.0"
description = "Answers once, then finishes."
entry_stage = "work"

[stages.work]
mode = "autonomous"
model = { provider = "seam", model = "m" }
description = "Answer"
system_prompt = "Answer, then stop."

[context.regions]
task = { kind = "pinned", max_tokens = 500, seed = "task" }
conversation = { kind = "sliding_window", max_items = 20, max_tokens = 10000 }
"#
}

/// The standing chain, held together so a dropped temp dir cannot pull the
/// socket out from under a live connection.
struct Seam {
    /// A client onto the daemon's control socket, for spawning.
    control: ControlClient,
    /// Where the websocket route is listening.
    addr: std::net::SocketAddr,
    /// The host's event sender, kept only to ask how many subscribers it has:
    /// one means the relay's `Subscribe` connection is live, which is the
    /// readiness signal that makes a sleep unnecessary here.
    events: broadcast::Sender<WorldEvent>,
    _dir: tempfile::TempDir,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// Stand up the production chain: host, control socket, event relay, `/ws`.
async fn stand_up(runs_dir: &std::path::Path) -> Seam {
    let mut providers = ProviderRegistry::new();
    providers.register("seam".to_string(), Arc::new(AnswersOnce));

    let mut host = crate::daemon::setup::build_host(crate::daemon::setup::HostParts {
        config: Config::default(),
        providers,
        runs_dir: runs_dir.to_path_buf(),
        shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
        mcp_tool_defs: vec![],
        mcp_tool_owners: Default::default(),
        mcp_pool: crate::daemon::mcp_pool::McpPool::for_daemon(
            Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            &[],
        ),
        runtime: tokio::runtime::Handle::current(),
        // A fixed clock, so nothing here is a function of how long CI took.
        now_secs: || 1_700_000_000,
        reloader: None,
    });

    // The daemon half: the host's own event sender served over a real control
    // socket, wired the way `main.rs` wires it.
    let events = host.event_sender();
    let served = events.clone();
    let (op_tx, op_rx) = tokio::sync::mpsc::unbounded_channel();
    let dir = tempfile::tempdir().expect("socket dir");
    let id = control_id(dir.path());
    let mut listener = bind_control_listener(&id).expect("bind the control socket");
    let token = ControlToken::create(dir.path()).expect("mint a token");
    let identity = crate::test_support::same_code_daemon(std::process::id());
    let accept = tokio::spawn(async move {
        while let Ok(Some(stream)) = listener.accept().await {
            let op_tx = op_tx.clone();
            let events = served.clone();
            let token = token.clone();
            let identity = identity.clone();
            tokio::spawn(async move {
                let _ = handle_connection_as(stream, op_tx, events, Some(token), identity).await;
            });
        }
    });
    let host_task = tokio::spawn(async move { host.serve(op_rx).await });

    // The gateway half: a client onto that socket, the relay that translates
    // world events into server events, and the route that fans them out.
    let control =
        ControlClient::for_home(id, dir.path()).with_build(crate::test_support::TEST_BUILD);
    let (event_tx, _) = broadcast::channel(256);
    let state = AppState {
        update_check: Default::default(),
        config: crate::commands::serve::testutil::fixed_config(Config::default()),
        event_tx,
        control: control.clone(),
        mcp: super::mcp::McpAdmin::default(),
        limits: Default::default(),
    };
    let relay = tokio::spawn(super::polling::event_loop(
        state.clone(),
        super::polling::RECONNECT_BACKOFF,
    ));

    let app = Router::new()
        .route("/ws", get(super::websocket::ws_global))
        .with_state(state);
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the websocket port");
    let addr = tcp.local_addr().expect("the bound address");
    let server = tokio::spawn(async move {
        let _ = axum::serve(tcp, app).await;
    });

    Seam {
        control,
        addr,
        events,
        _dir: dir,
        _tasks: vec![accept, host_task, relay, server],
    }
}

/// The `type` tag of one frame.
fn tag_of(frame: &serde_json::Value) -> &str {
    frame["type"].as_str().unwrap_or_default()
}

/// Read frames until every tag in `wanted` has arrived, returning all of them
/// in the order they came.
///
/// Every frame, not the first of each kind: the first `tokens` frame is the
/// zero baseline the change-detection pass emits when a run first appears, so
/// a first-wins collector would assert against a run that had not yet been
/// consulted. One timeout over the whole collection rather than a margin per
/// frame, because a per-frame margin is the shape that turns into a flake on a
/// loaded CI box.
async fn collect_frames(client: &mut WsTestClient, wanted: &[&str]) -> Vec<serde_json::Value> {
    let mut frames: Vec<serde_json::Value> = Vec::new();
    while !wanted
        .iter()
        .all(|w| frames.iter().any(|f| tag_of(f) == *w))
    {
        // The deadline is per frame rather than one over the whole collection,
        // so the failure names what had arrived by the time it gave up. A run
        // that produces nothing trips the first wait; a run that stalls
        // half-way trips a later one and says which half.
        let mut arrived: Vec<&str> = frames.iter().map(tag_of).collect();
        arrived.sort_unstable();
        arrived.dedup();
        let stuck = format!("never saw every frame; wanted {wanted:?}, got {arrived:?}");
        let (opcode, payload) =
            tokio::time::timeout(std::time::Duration::from_secs(30), client.recv_frame())
                .await
                .expect(&stuck);
        // Pings share the connection; only a text frame carries an event.
        if opcode == 0x1 {
            frames.push(serde_json::from_slice(&payload).expect("every frame is JSON"));
        }
    }
    frames
}

/// The last frame carrying `tag`, which for a run that has finished is its
/// settled value.
fn last<'a>(frames: &'a [serde_json::Value], tag: &str) -> &'a serde_json::Value {
    frames
        .iter()
        .rev()
        .find(|f| tag_of(f) == tag)
        .expect("collect_frames waits for every tag before returning")
}

/// The whole chain, asserted on the frames a subscriber actually receives.
///
/// This is the test issue #502 asked for. Its claim was that a run's status and
/// token changes have no producer, so no client can see them; what settles that
/// is a real run over a real socket, not a grep.
#[tokio::test]
async fn a_real_run_reaches_a_websocket_subscriber() {
    let agent_dir = tempfile::tempdir().expect("agent dir");
    let manifest = agent_dir.path().join("agent.leviath");
    std::fs::write(&manifest, one_stage_manifest()).expect("write manifest");
    let workdir = tempfile::tempdir().expect("workdir");
    let runs = tempfile::tempdir().expect("runs dir");

    let seam = stand_up(runs.path()).await;
    let mut client = WsTestClient::connect(seam.addr, "/ws").await;

    // Spawn only once the relay's `Subscribe` is live, so nothing under test
    // has already happened by the time anyone is listening. A receiver on the
    // host's broadcast channel is exactly that connection: nothing else in
    // this chain subscribes.
    let events = seam.events.clone();
    leviath_testkit::wait_until("the relay subscribed to the daemon", || {
        events.receiver_count() > 0
    })
    .await;

    let reply = seam
        .control
        .spawn(SpawnArgs {
            run_id: "seam-1".to_string(),
            blueprint_path: manifest.to_string_lossy().to_string(),
            task: "say something".to_string(),
            workdir: workdir.path().to_string_lossy().to_string(),
            ..Default::default()
        })
        .await
        .expect("the daemon answered the spawn");
    assert_eq!(
        reply,
        ControlResponse::Spawned {
            run_id: "seam-1".to_string()
        },
        "the spawn was refused"
    );

    let frames = collect_frames(
        &mut client,
        &["agent_spawned", "agent_status", "tokens", "agent_completed"],
    )
    .await;

    // The spawn frame. `parent_id` is null for a root run, and present.
    let spawned = last(&frames, "agent_spawned");
    assert_eq!(spawned["run_id"], "seam-1");
    assert_eq!(spawned["blueprint"], "seam");
    assert_eq!(spawned["parent_id"], serde_json::Value::Null);

    // The frame issue #502 says cannot exist. Its fields are asserted, not just
    // its tag: a status with no stage tells a console nothing. And the run
    // moved more than once, which is the other half of the claim - a client
    // that only ever hears one status has no more than it would have got from
    // the spawn.
    let statuses: Vec<&serde_json::Value> = frames
        .iter()
        .filter(|f| tag_of(f) == "agent_status")
        .collect();
    assert!(
        statuses.len() > 1,
        "a run that spawned, ran and finished sent {} status frames",
        statuses.len()
    );
    let status = last(&frames, "agent_status");
    assert_eq!(status["run_id"], "seam-1");
    assert_eq!(status["stage"], "work");
    assert_eq!(status["status"], "complete");
    assert!(status["accepts_messages"].is_boolean());

    // And the other one, carrying what the provider actually reported. A lower
    // bound, not an equality: a run makes more than one provider call (the
    // one-shot title generation is its own), so an exact total is a trap that
    // fails the day another call joins.
    let tokens = last(&frames, "tokens");
    assert_eq!(tokens["run_id"], "seam-1");
    assert!(
        tokens["prompt_tokens"].as_u64().unwrap_or(0) >= 7,
        "the token frame never carried the provider's usage: {tokens}"
    );
    assert!(tokens["completion_tokens"].as_u64().unwrap_or(0) >= 3);

    let completed = last(&frames, "agent_completed");
    assert_eq!(completed["run_id"], "seam-1");
    assert_eq!(completed["status"], "complete");

    client.send_close().await;
}
