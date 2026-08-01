//! The former single-file test module, exercising every pipeline section.
//! `use super::*;` sees the whole pipeline surface through mod.rs's
//! re-exports, exactly as it did when the sections were inline.

use super::*;
use crate::inference_pool::{InferencePoolConfig, InferencePools};
use leviath_core::{Region, RegionKind};
use tokio::sync::mpsc;

/// A provider whose capabilities can be toggled for the temperature branch.
struct Cfg {
    supports_temperature: bool,
    max_output: usize,
}
#[async_trait::async_trait]
impl Provider for Cfg {
    async fn infer(
        &self,
        _r: InferenceRequest,
    ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
        Ok(leviath_providers::InferenceResponse {
            content: "ok".to_string(),
            tool_calls: vec![],
            tokens_used: leviath_providers::TokenUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            finish_reason: leviath_providers::FinishReason::Complete,
        })
    }
    async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _m: &str) -> usize {
        100_000
    }
    fn name(&self) -> &str {
        "cfg"
    }
    fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
        leviath_providers::ModelCapabilities {
            supports_temperature: self.supports_temperature,
            max_output_tokens: self.max_output,
            ..Default::default()
        }
    }
}

fn window() -> ContextWindow {
    let mut w = ContextWindow::new(10_000);
    w.add_region(Region::new("sys".to_string(), RegionKind::Pinned, 1000));
    w
}

fn tool(name: &str) -> Tool {
    Tool {
        name: name.to_string(),
        description: String::new(),
        parameters: serde_json::Value::Null,
    }
}

fn stage(model: &str, tools: Vec<Tool>, filter: Option<Vec<String>>) -> StageInference {
    StageInference {
        provider_name: "cfg".to_string(),
        model: model.to_string(),
        tools,
        tool_filter: filter,
    }
}

fn provider(supports_temperature: bool, max_output: usize) -> Arc<dyn Provider> {
    Arc::new(Cfg {
        supports_temperature,
        max_output,
    })
}

// ── build_request branch coverage ──

#[test]
fn build_request_threads_stage_meta_into_custom_region_render() {
    // The custom region's script echoes the stage metadata build_request
    // passes - proving the dispatch wiring (stage name, per-stage iteration,
    // model) reaches render(ctx).
    let mut w = ContextWindow::new(10_000);
    w.add_region(Region::new(
        "brain".to_string(),
        RegionKind::Custom {
            script: "meta.rhai".to_string(),
            persistent: false,
        },
        1_000,
    ));
    w.region_scripts.insert(
        "meta.rhai".to_string(),
        Arc::new(
            leviath_scripting::region_hook::compile(
                "meta.rhai",
                "fn render(ctx) { `${ctx.stage_name}#${ctx.stage_iterations}@${ctx.model}` }",
            )
            .unwrap(),
        ),
    );
    let si = stage("model-x", vec![], None);
    let req = build_request(&w, None, &si, &provider(true, 500), "implement", 4);
    assert!(
        req.system.iter().any(|b| b.text == "implement#4@model-x"),
        "system blocks: {:?}",
        req.system.iter().map(|b| &b.text).collect::<Vec<_>>()
    );
}

#[test]
fn build_request_filters_tools_and_uses_config_overrides() {
    let cfg = InferenceConfig {
        temperature: Some(0.1),
        max_output_tokens: Some(42),
        extra_params: Default::default(),
        batch_tool_hint: false,
        request_timeout_secs: None,
    };
    let si = stage(
        "m",
        vec![tool("keep"), tool("drop")],
        Some(vec!["keep".into()]),
    );
    let req = build_request(
        &window(),
        Some(&cfg),
        &si,
        &provider(true, 9999),
        "test-stage",
        0,
    );
    assert_eq!(req.tools.len(), 1); // filtered to "keep"
    assert_eq!(req.tools[0].name, "keep");
    assert_eq!(req.max_tokens, 42); // config output cap wins
    assert_eq!(req.temperature, 0.1); // config temperature
    assert_eq!(req.extra, serde_json::Value::Null); // no extra params → Null
    assert_eq!(req.request_timeout_secs, None); // unset config → no per-call cap
}

#[test]
fn build_request_threads_per_stage_timeout() {
    // A stage's request_timeout_secs is carried onto the request so the
    // provider can bound the call; absent config yields None.
    let cfg = InferenceConfig {
        request_timeout_secs: Some(120),
        ..Default::default()
    };
    let si = stage("m", vec![], None);
    let req = build_request(
        &window(),
        Some(&cfg),
        &si,
        &provider(true, 500),
        "test-stage",
        0,
    );
    assert_eq!(req.request_timeout_secs, Some(120));

    let req_none = build_request(&window(), None, &si, &provider(true, 500), "test-stage", 0);
    assert_eq!(req_none.request_timeout_secs, None);
}

#[test]
fn build_request_passes_through_extra_params() {
    let mut extra_params = serde_json::Map::new();
    extra_params.insert("top_p".to_string(), serde_json::json!(0.9));
    let cfg = InferenceConfig {
        temperature: None,
        max_output_tokens: None,
        extra_params,
        batch_tool_hint: false,
        request_timeout_secs: None,
    };
    let si = stage("m", vec![], None);
    let req = build_request(
        &window(),
        Some(&cfg),
        &si,
        &provider(true, 500),
        "test-stage",
        0,
    );
    assert_eq!(req.extra, serde_json::json!({ "top_p": 0.9 }));
}

/// A window whose pinned region carries a real entry, so `assemble` yields a
/// non-empty `system` - required for the batch-hint tests to actually iterate
/// the assembled blocks (an empty `system` would skip every closure).
fn window_with_sys() -> ContextWindow {
    let mut w = window();
    w.add_to_region("sys", "base system instructions".to_string(), 6)
        .expect("seed pinned region");
    w
}

#[test]
fn build_request_prepends_batch_hint_when_enabled() {
    let cfg = InferenceConfig {
        temperature: None,
        max_output_tokens: None,
        extra_params: Default::default(),
        batch_tool_hint: true,
        request_timeout_secs: None,
    };
    let si = stage("m", vec![], None);
    let req = build_request(
        &window_with_sys(),
        Some(&cfg),
        &si,
        &provider(true, 500),
        "test-stage",
        0,
    );
    // The hint is prepended ahead of the stage's own system block(s).
    assert_eq!(
        req.system.first().map(|b| b.text.as_str()),
        Some(BATCH_TOOL_HINT)
    );
    assert_eq!(req.system[0].cache_hint, leviath_core::CacheHint::Always);
    assert!(
        req.system[1..]
            .iter()
            .any(|b| b.text.contains("base system")),
        "the stage's own system block is preserved after the hint"
    );
}

#[test]
fn build_request_omits_batch_hint_when_disabled_or_absent() {
    let si = stage("m", vec![], None);
    // Disabled via config.
    let cfg = InferenceConfig {
        temperature: None,
        max_output_tokens: None,
        extra_params: Default::default(),
        batch_tool_hint: false,
        request_timeout_secs: None,
    };
    let req = build_request(
        &window_with_sys(),
        Some(&cfg),
        &si,
        &provider(true, 500),
        "test-stage",
        0,
    );
    assert!(!req.system.is_empty());
    assert!(req.system.iter().all(|b| b.text != BATCH_TOOL_HINT));
    // Absent config → no hint.
    let req_none = build_request(
        &window_with_sys(),
        None,
        &si,
        &provider(true, 500),
        "test-stage",
        0,
    );
    assert!(!req_none.system.is_empty());
    assert!(req_none.system.iter().all(|b| b.text != BATCH_TOOL_HINT));
}

#[test]
fn build_request_all_tools_default_temperature_no_config() {
    let si = stage("m", vec![tool("a"), tool("b")], None); // None filter = all
    let req = build_request(&window(), None, &si, &provider(true, 500), "test-stage", 0);
    assert_eq!(req.tools.len(), 2);
    assert_eq!(req.temperature, 0.7); // default when supported and no config
    assert_eq!(req.max_tokens, 500); // capability cap when no config override
}

#[test]
fn build_request_empty_filter_is_all_and_no_temperature_when_unsupported() {
    let si = stage("m", vec![tool("a")], Some(vec![])); // empty filter = all
    let req = build_request(&window(), None, &si, &provider(false, 500), "test-stage", 0);
    assert_eq!(req.tools.len(), 1);
    assert_eq!(req.temperature, 0.0); // model doesn't support temperature
}

#[tokio::test]
async fn cfg_provider_metadata_is_exercised() {
    // Keep the mock's non-`infer`/`capabilities` trait methods measured.
    let p = Cfg {
        supports_temperature: true,
        max_output: 1,
    };
    assert_eq!(p.name(), "cfg");
    assert_eq!(p.count_tokens("t", "m").await, 1);
    assert_eq!(p.max_context_tokens("m"), 100_000);
}

// ── dispatch system ──

fn build_world(pools: InferencePools) -> (World, mpsc::UnboundedReceiver<InferenceOutcome>) {
    let mut registry = ProviderRegistry::new();
    registry.register("cfg".to_string(), provider(true, 1000));
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(Providers(registry));
    let (ttx, _trx) = mpsc::unbounded_channel();
    let (ctx, _crx) = mpsc::unbounded_channel();
    let (cstx, _csrx) = mpsc::unbounded_channel();
    world.insert_resource(InferenceStage {
        pools: Arc::new(pools),
        outcomes: tx,
        transition_outcomes: ttx,
        compaction_outcomes: ctx,
        content_summary_outcomes: cstx,
        wake: Arc::new(Notify::new()),
        runtime: Handle::current(),
        exact_token_counting: false,
    });
    (world, rx)
}

fn run(world: &mut World) {
    let mut schedule = Schedule::default();
    schedule.add_systems(dispatch_inference);
    schedule.run(world);
}

#[tokio::test]
async fn dispatch_moves_agent_to_awaiting_and_runs_the_job() {
    let (mut world, mut rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let e = world
        .spawn((
            agent_state(),
            window(),
            stage("m", vec![], None),
            ReadyToInfer,
        ))
        .id();

    run(&mut world);

    // Phase advanced.
    assert!(world.get::<AwaitingInference>(e).is_some());
    assert!(world.get::<ReadyToInfer>(e).is_none());
    // The spawned job ran and reported an outcome.
    let outcome = rx.recv().await.expect("outcome");
    assert_eq!(outcome.entity, e);
    assert!(outcome.result.is_ok());
}

#[tokio::test]
async fn dispatch_skips_when_pool_full() {
    let mut cfg = InferencePoolConfig::new();
    cfg.set_limit("m", 1);
    let pools = InferencePools::new(cfg);
    let _held = pools.try_acquire("m").unwrap(); // occupy the only slot
    let (mut world, _rx) = build_world(pools);
    let e = world
        .spawn((
            agent_state(),
            window(),
            stage("m", vec![], None),
            ReadyToInfer,
        ))
        .id();

    run(&mut world);

    // No slot ⇒ still ready, not dispatched.
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<AwaitingInference>(e).is_none());
}

#[tokio::test]
async fn dispatch_skips_when_provider_missing() {
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let e = world
        .spawn((
            agent_state(),
            window(),
            stage("m", vec![], None).clone_with_provider("nope"),
            ReadyToInfer,
        ))
        .id();

    run(&mut world);

    assert!(world.get::<ReadyToInfer>(e).is_some()); // unknown provider ⇒ untouched
    assert!(world.get::<AwaitingInference>(e).is_none());
}

#[tokio::test]
async fn dispatch_inference_skips_non_active_agent() {
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let mut st = agent_state();
    st.status = AgentStatus::Idle; // paused
    let e = world
        .spawn((st, window(), stage("m", vec![], None), ReadyToInfer))
        .id();

    run(&mut world);

    // Paused ⇒ not dispatched, stays ready for when it resumes.
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<AwaitingInference>(e).is_none());
}

impl StageInference {
    fn clone_with_provider(mut self, name: &str) -> Self {
        self.provider_name = name.to_string();
        self
    }
}

// ── collect system ──

fn agent_state() -> AgentState {
    AgentState {
        agent_id: "a".to_string(),
        current_stage: "s".to_string(),
        iteration: 0,
        status: AgentStatus::Active,
        spawned_children_ids: vec![],
        pending_wait: None,
        accepts_messages: true,
    }
}

fn resp(text: &str) -> leviath_providers::InferenceResponse {
    leviath_providers::InferenceResponse {
        content: text.to_string(),
        tool_calls: vec![],
        tokens_used: leviath_providers::TokenUsage {
            prompt_tokens: 1,
            completion_tokens: 1,
            total_tokens: 2,
            cached_tokens: 0,
            cache_write_tokens: 0,
        },
        finish_reason: leviath_providers::FinishReason::Complete,
    }
}

fn run_collect(world: &mut World) {
    let mut schedule = Schedule::default();
    schedule.add_systems(collect_inference);
    schedule.run(world);
}

fn world_with_results() -> (World, mpsc::UnboundedSender<InferenceOutcome>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(InferenceResults(rx));
    (world, tx)
}

#[test]
fn collect_applies_ok_and_advances_to_process_response() {
    let (mut world, tx) = world_with_results();
    let e = world.spawn((agent_state(), AwaitingInference)).id();
    let mut response = resp("hi");
    response.tool_calls.push(leviath_providers::ToolCall {
        id: "call-1".to_string(),
        name: "read_file".to_string(),
        arguments: serde_json::json!({"path": "x"}),
        thought_signature: None,
    });
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Ok(response),
    })
    .unwrap();

    run_collect(&mut world);

    assert!(world.get::<ProcessResponse>(e).is_some());
    assert!(world.get::<AwaitingInference>(e).is_none());
    assert_eq!(world.get::<AgentState>(e).unwrap().iteration, 1);
    let stored = world.get::<crate::components::InferenceResult>(e).unwrap();
    assert_eq!(stored.response, "hi");
    // The tool call was mapped onto the stored result.
    assert_eq!(stored.tool_calls.len(), 1);
    assert_eq!(stored.tool_calls[0].name, "read_file");
}

#[test]
fn collect_marks_error_on_failure() {
    let (mut world, tx) = world_with_results();
    let e = world.spawn((agent_state(), AwaitingInference)).id();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Err(leviath_providers::ProviderError::Other("boom".to_string())),
    })
    .unwrap();

    run_collect(&mut world);

    // `ProviderError::Other`'s Display is the inner message ("boom").
    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Error {
            message: "boom".to_string()
        }
    );
    assert!(world.get::<AwaitingInference>(e).is_none());
    // The error is routed to the transition logic (which follows an `error`
    // edge if the stage has one, else terminates).
    assert!(world.get::<ResolveTransition>(e).is_some());
    assert_eq!(
        world.get::<StageOutcome>(e).unwrap(),
        &StageOutcome::Errored("boom".to_string())
    );
}

// ── stage-io persistence (#1) ──

fn ledger2() -> StageLedger {
    StageLedger(vec![
        leviath_core::run_meta::StageRecord::new("plan".to_string(), 0),
        leviath_core::run_meta::StageRecord::new("impl".to_string(), 1),
    ])
}

#[test]
fn one_line_collapses_whitespace_and_truncates() {
    assert_eq!(one_line("a\n  b\tc ", 100), "a b c");
    let long = "x".repeat(250);
    let out = one_line(&long, 200);
    assert!(out.ends_with('…'));
    assert_eq!(out.chars().count(), 201); // 200 chars + the ellipsis
}

#[test]
fn reconcile_stage_ledger_sets_past_active_future_once() {
    use leviath_core::run_meta::StageRunStatus;
    let mut led = StageLedger(vec![
        leviath_core::run_meta::StageRecord::new("a".to_string(), 0),
        leviath_core::run_meta::StageRecord::new("b".to_string(), 1),
        leviath_core::run_meta::StageRecord::new("c".to_string(), 2),
    ]);
    reconcile_stage_ledger(&mut led, 1, &AgentStatus::Active, 100);
    assert_eq!(led.0[0].status, StageRunStatus::Complete);
    assert_eq!(led.0[0].started_at, Some(100));
    assert_eq!(led.0[0].ended_at, Some(100));
    assert_eq!(led.0[1].status, StageRunStatus::Active);
    assert_eq!(led.0[1].started_at, Some(100));
    assert_eq!(led.0[1].ended_at, None);
    assert_eq!(led.0[2].status, StageRunStatus::Pending);

    // Idempotent: a later reconcile doesn't overwrite the stamped timestamps.
    reconcile_stage_ledger(&mut led, 1, &AgentStatus::Active, 200);
    assert_eq!(led.0[0].ended_at, Some(100));
    assert_eq!(led.0[1].started_at, Some(100));
}

#[test]
fn reconcile_stage_ledger_completes_current_stage_on_run_complete() {
    use leviath_core::run_meta::StageRunStatus;
    let mut led = StageLedger(vec![leviath_core::run_meta::StageRecord::new(
        "a".to_string(),
        0,
    )]);
    reconcile_stage_ledger(&mut led, 0, &AgentStatus::Complete, 50);
    assert_eq!(led.0[0].status, StageRunStatus::Complete);
    assert_eq!(led.0[0].ended_at, Some(50));
}

#[test]
fn collect_inference_buffers_output_token_line_and_stage_tokens() {
    let (mut world, tx) = world_with_results();
    let e = world
        .spawn((
            agent_state(),
            AwaitingInference,
            StageCursor { index: 1 },
            ledger2(),
            StageIoBuffer::default(),
        ))
        .id();
    let mut response = resp("the plan");
    response.tokens_used.prompt_tokens = 5;
    response.tokens_used.completion_tokens = 3;
    response.tokens_used.cached_tokens = 2;
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Ok(response),
    })
    .unwrap();

    run_collect(&mut world);

    let buf = world.get::<StageIoBuffer>(e).unwrap();
    assert_eq!(buf.output, vec![(1, "the plan".to_string())]);
    assert_eq!(buf.logs, vec![(1, "[Tokens: 5 in, 3 out]".to_string())]);
    let led = world.get::<StageLedger>(e).unwrap();
    assert_eq!(led.0[1].prompt_tokens, 5);
    assert_eq!(led.0[1].completion_tokens, 3);
    assert_eq!(led.0[1].cached_tokens, 2);
}

// ─── abort_terminal_work ───

fn run_abort(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(abort_terminal_work);
    s.run(world);
}

/// `track_in_flight` accumulates rather than replaces, so an agent that has
/// something outstanding when a second job is dispatched keeps both handles -
/// dropping the first would make that job uncancellable.
#[test]
fn track_in_flight_accumulates_across_dispatches() {
    fn add_one(agents: Query<(Entity, Option<&InFlightWork>)>, mut commands: Commands) {
        for (entity, existing) in agents.iter() {
            track_in_flight(
                &mut commands,
                entity,
                existing,
                crate::cancel::CancelToken::new(),
            );
        }
    }

    let mut world = World::new();
    let e = world.spawn(agent_state()).id();
    let mut schedule = Schedule::default();
    schedule.add_systems(add_one);

    schedule.run(&mut world); // no existing component yet
    assert_eq!(world.get::<InFlightWork>(e).unwrap().0.len(), 1);

    schedule.run(&mut world); // one already attached
    assert_eq!(
        world.get::<InFlightWork>(e).unwrap().0.len(),
        2,
        "the earlier job's handle is kept"
    );
}

#[test]
fn abort_terminal_work_stops_a_cancelled_agents_in_flight_work() {
    for status in [
        AgentStatus::Cancelled,
        AgentStatus::Complete,
        AgentStatus::Error {
            message: "boom".to_string(),
        },
    ] {
        let mut world = World::new();
        let tokens = vec![
            crate::cancel::CancelToken::new(),
            crate::cancel::CancelToken::new(),
        ];
        let mut state = agent_state();
        state.status = status.clone();
        let e = world.spawn((state, InFlightWork(tokens.clone()))).id();

        run_abort(&mut world);

        assert!(
            tokens.iter().all(|t| t.is_cancelled()),
            "{status:?} stops every in-flight job"
        );
        assert!(
            world.get::<InFlightWork>(e).is_none(),
            "and the handles are dropped"
        );
    }
}

#[test]
fn abort_terminal_work_leaves_a_running_agent_alone() {
    let mut world = World::new();
    let token = crate::cancel::CancelToken::new();
    let e = world
        .spawn((agent_state(), InFlightWork(vec![token.clone()])))
        .id();

    run_abort(&mut world);

    assert!(!token.is_cancelled(), "an Active agent keeps working");
    assert!(world.get::<InFlightWork>(e).is_some());
}

/// A response that lands after the run was cancelled is discarded. The
/// dispatch guard stops *new* inferences, but one already in flight still
/// returns - and applying it advanced the run to `ProcessResponse`, from
/// which it carried on as if nothing had happened.
#[test]
fn collect_inference_drops_a_response_for_a_cancelled_run() {
    let (mut world, tx) = world_with_results();
    let mut state = agent_state();
    state.status = AgentStatus::Cancelled;
    let e = world
        .spawn((
            state,
            AwaitingInference,
            StageCursor { index: 0 },
            StageIoBuffer::default(),
        ))
        .id();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Ok(resp("too late")),
    })
    .unwrap();

    run_collect(&mut world);

    let state = world.get::<AgentState>(e).unwrap();
    assert_eq!(state.status, AgentStatus::Cancelled, "stays cancelled");
    assert_eq!(state.iteration, 0, "the response was not counted");
    assert!(
        world.get::<ProcessResponse>(e).is_none(),
        "and the run is not advanced by it"
    );
    assert!(
        world.get::<AwaitingInference>(e).is_none(),
        "the awaiting marker is cleared so nothing re-collects it"
    );
}

#[test]
fn collect_inference_skips_empty_output_but_logs_tokens() {
    let (mut world, tx) = world_with_results();
    let e = world
        .spawn((
            agent_state(),
            AwaitingInference,
            StageCursor { index: 0 },
            StageIoBuffer::default(),
        ))
        .id();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Ok(resp("   ")), // whitespace-only ⇒ no output line
    })
    .unwrap();

    run_collect(&mut world);

    let buf = world.get::<StageIoBuffer>(e).unwrap();
    assert!(buf.output.is_empty());
    assert_eq!(buf.logs.len(), 1); // token line only
}

#[test]
fn collect_inference_error_buffers_error_line() {
    let (mut world, tx) = world_with_results();
    let e = world
        .spawn((
            agent_state(),
            AwaitingInference,
            StageCursor { index: 0 },
            StageIoBuffer::default(),
        ))
        .id();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Err(leviath_providers::ProviderError::Other("boom".to_string())),
    })
    .unwrap();

    run_collect(&mut world);

    let buf = world.get::<StageIoBuffer>(e).unwrap();
    assert_eq!(buf.logs, vec![(0, "[error] boom".to_string())]);
}

#[test]
fn collect_inference_tolerates_cursor_beyond_ledger() {
    let (mut world, tx) = world_with_results();
    let e = world
        .spawn((
            agent_state(),
            AwaitingInference,
            StageCursor { index: 9 }, // past the 2-stage ledger
            ledger2(),
            StageIoBuffer::default(),
        ))
        .id();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Ok(resp("x")),
    })
    .unwrap();

    run_collect(&mut world);

    // No panic; output tagged with idx 9, ledger tokens untouched.
    assert_eq!(
        world.get::<StageIoBuffer>(e).unwrap().output,
        vec![(9, "x".to_string())]
    );
    assert_eq!(world.get::<StageLedger>(e).unwrap().0[0].prompt_tokens, 0);
}

#[test]
fn collect_tools_buffers_one_tool_log_line_per_call() {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolResults(rx));
    let e = world
        .spawn((
            ctx(&[("conversation", 10_000)]),
            infer_with(vec![tc("c1", "read_file")]),
            AwaitingTools,
            StageCursor { index: 2 },
            StageIoBuffer::default(),
        ))
        .id();
    tx.send(ToolOutcome {
        elapsed: std::time::Duration::ZERO,
        entity: e,
        results: vec![("c1".to_string(), "file\nbody".to_string())],
    })
    .unwrap();

    run_collect_tools(&mut world);

    let buf = world.get::<StageIoBuffer>(e).unwrap();
    assert_eq!(
        buf.logs,
        vec![(2, "[tool] read_file: file body".to_string())]
    );
}

#[test]
fn dispatch_persistence_emits_stage_index_and_drains_io_buffer() {
    use leviath_core::run_meta::StageRunStatus;
    let (mut world, mut rx) = world_with_persistence();
    let mut buf = StageIoBuffer::default();
    buf.output.push((0, "hello".to_string()));
    buf.logs.push((0, "[tool] x: y".to_string()));
    let e = world
        .spawn((
            run_metadata(),
            agent_state(),
            conv_window(),
            StageCursor { index: 0 },
            TokenTotals::default(),
            PersistWatermark::default(),
            ledger2(),
            buf,
        ))
        .id();

    run_dispatch_persistence(&mut world);

    let job = rx.try_recv().expect("job sent");
    assert_eq!(job.stages.len(), 2);
    assert_eq!(job.stages[0].name, "plan");
    assert_eq!(job.stages[0].status, StageRunStatus::Active);
    assert_eq!(job.output_appends, vec![(0, "hello".to_string())]);
    assert_eq!(job.log_appends, vec![(0, "[tool] x: y".to_string())]);
    // The buffer was drained in place.
    assert!(world.get::<StageIoBuffer>(e).unwrap().output.is_empty());
}

#[test]
fn dispatch_persistence_records_tree_links() {
    use crate::components::{ParentRef, SubAgentChildren};
    let (mut world, mut rx) = world_with_persistence();
    let child = world.spawn_empty().id();
    let mut state = agent_state();
    state.spawned_children_ids = vec!["kid-1".to_string()];
    world.spawn((
        run_metadata(),
        state,
        conv_window(),
        StageCursor { index: 0 },
        TokenTotals::default(),
        PersistWatermark::default(),
        ParentRef {
            parent_entity: child,
            parent_agent_id: "p".to_string(),
            depth: 3,
        },
        SubAgentChildren {
            children: vec![child],
            max_child_depth: 6,
        },
    ));

    run_dispatch_persistence(&mut world);

    let job = rx.try_recv().expect("job sent");
    // The persisted meta carries the tree links for a deterministic restore.
    assert_eq!(job.meta.children, vec!["kid-1".to_string()]);
    assert_eq!(job.meta.depth, 3);
    assert_eq!(job.meta.max_child_depth, 6);
}

#[test]
fn dispatch_persistence_serializes_fan_out_waiting() {
    use leviath_core::blueprint::{FanOutConfig, WorkerFailurePolicy};
    let (mut world, mut rx) = world_with_persistence();
    let e = world
        .spawn((
            run_metadata(),
            agent_state(),
            conv_window(),
            StageCursor { index: 0 },
            TokenTotals::default(),
            PersistWatermark::default(),
        ))
        .id();
    // Attach a (minimal) FanOutWaiting via the public restore path.
    crate::fanout::restore_fan_out_waiting(
        &mut world,
        e,
        crate::fanout::FanOutState {
            config: FanOutConfig {
                worker_agent: None,
                worker_stage: Some("w".to_string()),
                worker_query: None,
                merge_stage: None,
                max_workers: 1,
                on_worker_failure: WorkerFailurePolicy::Continue,
                split_prompt: "s".to_string(),
            },
            max_workers: 1,
            pending: vec![],
            active: vec![],
            summaries: vec![],
            failures: vec![],
        },
        &|_| None,
    );

    run_dispatch_persistence(&mut world);
    let job = rx.try_recv().expect("job sent");
    assert!(job.fanout.is_some(), "fan-out waiting state persisted");
}

#[tokio::test]
async fn dispatch_persistence_serializes_interaction_point() {
    use crate::dynamic_interaction::InteractionBackend;
    let (mut world, mut rx) = world_with_persistence();
    let hub = InteractionHub::new();
    world.insert_resource(hub.clone());
    world.spawn((
        run_metadata(),
        agent_state(), // agent_id = "a"
        conv_window(),
        StageCursor { index: 0 },
        TokenTotals::default(),
        PersistWatermark::default(),
        crate::interaction_points::AwaitingInteractionPoint,
        crate::interaction_points::InteractionPointCursor(1),
        crate::interaction_points::InteractionPointRounds(3),
    ));

    // Open the point request for this agent in the hub, carrying the document.
    let backend = hub.backend_for("a".to_string());
    let ask = tokio::spawn(async move {
        let mut req = leviath_core::interaction::InteractionRequest::multiple_choice(
            "a-point-plan_approval-3",
            "Approve?",
            vec!["Approve".to_string(), "Abort".to_string()],
            "plan",
        );
        req.body = Some("the plan".to_string());
        backend.ask(req).await
    });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    run_dispatch_persistence(&mut world);
    let job = rx.try_recv().expect("job sent");
    let json = job.interactions.expect("interaction-point state persisted");
    let state: crate::interaction_points::InteractionPointState =
        serde_json::from_str(&json).unwrap();
    assert_eq!(state.cursor, 1);
    assert_eq!(state.round, 3);
    assert_eq!(state.body, "the plan");

    // Let the still-blocked ask complete so its task ends cleanly.
    assert!(
        hub.answer(leviath_core::interaction::InteractionResponse::text(
            "a-point-plan_approval-3",
            "",
        ))
    );
    ask.await.unwrap();
}

#[test]
fn dispatch_persistence_omits_interactions_when_not_at_a_point() {
    let (mut world, mut rx) = world_with_persistence();
    world.spawn((
        run_metadata(),
        agent_state(),
        conv_window(),
        StageCursor { index: 0 },
        TokenTotals::default(),
        PersistWatermark::default(),
    ));
    run_dispatch_persistence(&mut world);
    let job = rx.try_recv().expect("job sent");
    assert!(job.interactions.is_none());
}

#[test]
fn dispatch_persistence_omits_interactions_without_a_hub() {
    // Awaiting a point but no hub resource (e.g. a test world) ⇒ nothing to read
    // the open request from, so no sidecar is written.
    let (mut world, mut rx) = world_with_persistence();
    world.spawn((
        run_metadata(),
        agent_state(),
        conv_window(),
        StageCursor { index: 0 },
        TokenTotals::default(),
        PersistWatermark::default(),
        crate::interaction_points::AwaitingInteractionPoint,
    ));
    run_dispatch_persistence(&mut world);
    assert!(rx.try_recv().expect("job sent").interactions.is_none());
}

#[test]
fn dispatch_persistence_omits_interactions_when_request_not_yet_registered() {
    // Awaiting a point with a hub present, but the ask task hasn't registered the
    // request yet ⇒ skip this tick (the next persist captures it).
    let (mut world, mut rx) = world_with_persistence();
    world.insert_resource(InteractionHub::new()); // empty
    world.spawn((
        run_metadata(),
        agent_state(),
        conv_window(),
        StageCursor { index: 0 },
        TokenTotals::default(),
        PersistWatermark::default(),
        crate::interaction_points::AwaitingInteractionPoint,
    ));
    run_dispatch_persistence(&mut world);
    assert!(rx.try_recv().expect("job sent").interactions.is_none());
}

#[test]
fn dispatch_persistence_flushes_buffered_io_without_a_watermark_change() {
    let (mut world, mut rx) = world_with_persistence();
    let e = world
        .spawn((
            run_metadata(),
            agent_state(),
            conv_window(),
            StageCursor { index: 0 },
            TokenTotals::default(),
            PersistWatermark::default(),
            StageIoBuffer::default(),
        ))
        .id();

    // First pass: watermark changes ⇒ a job is sent, buffer stays empty.
    run_dispatch_persistence(&mut world);
    let _ = rx.try_recv().expect("first job");

    // Watermark unchanged, but new buffered content ⇒ still flushed.
    world
        .get_mut::<StageIoBuffer>(e)
        .unwrap()
        .logs
        .push((0, "late log".to_string()));
    run_dispatch_persistence(&mut world);
    let job = rx.try_recv().expect("append-triggered job");
    assert_eq!(job.log_appends, vec![(0, "late log".to_string())]);
}

#[test]
fn dispatch_persistence_broadcasts_buffered_lines_as_log_events() {
    use crate::host::{WorldEvent, WorldEventSink};
    let (mut world, _rx) = world_with_persistence();
    let (sink_tx, mut sink_rx) = tokio::sync::broadcast::channel(16);
    world.insert_resource(WorldEventSink(sink_tx));
    let mut buf = StageIoBuffer::default();
    buf.output.push((0, "readable output".to_string()));
    buf.logs.push((0, "[Tokens: 1 in, 2 out]".to_string()));
    world.spawn((
        run_metadata(),
        agent_state(),
        conv_window(),
        StageCursor { index: 0 },
        TokenTotals::default(),
        PersistWatermark::default(),
        buf,
    ));

    run_dispatch_persistence(&mut world);

    // Output lines stream first, then operational logs - each as a `Log`
    // carrying the agent's run/agent ids and the raw line.
    let first = sink_rx.try_recv().expect("output log event");
    assert_eq!(
        first,
        WorldEvent::Log {
            run_id: "run-1".to_string(),
            agent_id: "a".to_string(),
            line: "readable output".to_string(),
        }
    );
    let second = sink_rx.try_recv().expect("operational log event");
    assert_eq!(
        second,
        WorldEvent::Log {
            run_id: "run-1".to_string(),
            agent_id: "a".to_string(),
            line: "[Tokens: 1 in, 2 out]".to_string(),
        }
    );
    assert!(sink_rx.try_recv().is_err(), "no extra events");
}

#[test]
fn dispatch_persistence_emits_no_log_events_without_a_sink() {
    use crate::host::WorldEventSink;
    let (mut world, _rx) = world_with_persistence();
    // A sink whose sender is *not* installed as a world resource: the system
    // can't reach it, so nothing is broadcast.
    let (sink_tx, mut sink_rx) = tokio::sync::broadcast::channel(16);
    let _keep_alive = WorldEventSink(sink_tx);
    let mut buf = StageIoBuffer::default();
    buf.output.push((0, "line".to_string()));
    world.spawn((
        run_metadata(),
        agent_state(),
        conv_window(),
        StageCursor { index: 0 },
        TokenTotals::default(),
        PersistWatermark::default(),
        buf,
    ));

    run_dispatch_persistence(&mut world);

    assert!(sink_rx.try_recv().is_err(), "no events without the sink");
}

#[test]
fn dispatch_persistence_persists_taint_audit_when_the_gate_has_events() {
    let (mut world, mut prx) = world_with_persistence();
    let (jtx, _jrx) = mpsc::unbounded_channel();
    world.insert_resource(ToolServiceRes(std::sync::Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    world.spawn((
        run_metadata(),
        agent_state(),
        infer_with(vec![tc("c_shell", "shell")]),
        tainted_conv_window(),
        ReadyForTools,
        enabled_gate(),
        StageCursor { index: 1 },
        TokenTotals::default(),
        PersistWatermark::default(),
    ));
    // Run the tool dispatch so the gate blocks the outbound call and records
    // an audit event, then persist.
    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);
    run_dispatch_persistence(&mut world);

    let job = prx.try_recv().expect("persist job");
    let (idx, json) = job.taint_audit.expect("taint audit persisted");
    assert_eq!(idx, 1);
    assert!(json.contains("shell"));
}

#[test]
fn dispatch_persistence_skips_taint_audit_when_the_gate_is_empty() {
    let (mut world, mut prx) = world_with_persistence();
    world.spawn((
        run_metadata(),
        agent_state(),
        conv_window(),
        StageCursor { index: 0 },
        TokenTotals::default(),
        PersistWatermark::default(),
        enabled_gate(), // no events recorded
    ));
    run_dispatch_persistence(&mut world);
    let job = prx.try_recv().expect("persist job");
    assert!(job.taint_audit.is_none());
}

#[test]
fn spawn_agent_seeds_the_stage_ledger_with_names() {
    let mk = |name: &str| {
        leviath_core::Stage::new(
            name.to_string(),
            leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
        )
    };
    let mut bp = blueprint(vec![mk("plan"), mk("build")]);
    bp.repetition_detection = Some(leviath_core::blueprint::RepetitionDetectionConfig {
        max_repeat_calls: Some(2),
        max_readonly_streak: None,
        enabled: Some(true),
    });
    let mut world = World::new();
    let e = spawn_agent(
        &mut world,
        "run-led".to_string(),
        bp,
        "task",
        vec![resolved("m"), resolved("m")],
        true,
    )
    .expect("spawn");
    let led = world.get::<StageLedger>(e).expect("ledger seeded");
    assert_eq!(led.0.len(), 2);
    assert_eq!(led.0[0].name, "plan");
    assert_eq!(led.0[1].name, "build");
    assert!(world.get::<StageIoBuffer>(e).is_some());
    // The repetition detector was seeded from the blueprint config.
    assert!(
        world
            .get::<crate::repetition::RepetitionDetector>(e)
            .is_some()
    );
}

fn percent_region_blueprint(percent: f64) -> leviath_core::Blueprint {
    let layout = leviath_core::layout::ContextLayout::new(
        vec![
            leviath_core::layout::RegionDefinition::new("sys".to_string(), RegionKind::Pinned, 0)
                .with_budget(leviath_core::BudgetSpec::Percent {
                    percent,
                    min: None,
                    max: None,
                }),
        ],
        0,
    );
    let stages = vec![leviath_core::Stage::new(
        "main".to_string(),
        leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
    )];
    leviath_core::Blueprint::new("t".to_string(), "d".to_string(), stages, layout)
}

fn world_with_provider() -> World {
    let mut world = World::new();
    let mut reg = ProviderRegistry::new();
    reg.register("p".to_string(), provider(true, 500));
    world.insert_resource(Providers(reg));
    world
}

#[test]
fn spawn_agent_seeded_resolves_percent_region_against_provider_window() {
    // Provider "p" (Cfg) reports a 100_000-token window; a 35% region must
    // resolve to 35_000, and the window total becomes the model window.
    let mut world = world_with_provider();
    let e = spawn_agent(
        &mut world,
        "run".to_string(),
        percent_region_blueprint(0.35),
        "task",
        vec![resolved("m")],
        true,
    )
    .expect("spawn");
    let w = world.get::<ContextWindow>(e).expect("window");
    assert_eq!(w.get_region("sys").unwrap().max_tokens, 35_000);
    assert_eq!(w.max_tokens, 100_000);
}

#[test]
fn spawn_agent_seeded_falls_back_when_provider_missing() {
    // No Providers resource → percentage resolves against the 8192 default
    // window (and warns). 35% of 8192 ≈ 2867.
    crate::test_support::with_tracing(|| {
        let mut world = World::new();
        let e = spawn_agent(
            &mut world,
            "run".to_string(),
            percent_region_blueprint(0.35),
            "task",
            vec![resolved("m")],
            true,
        )
        .expect("spawn");
        let w = world.get::<ContextWindow>(e).expect("window");
        let expected = (8192f64 * 0.35).round() as usize;
        assert_eq!(w.get_region("sys").unwrap().max_tokens, expected);
        assert_eq!(w.max_tokens, DEFAULT_CONTEXT_WINDOW_TOKENS);
    });
}

#[test]
fn spawn_agent_seeded_absolute_blueprint_is_unchanged() {
    // A pure-absolute blueprint resolves to itself: region max_tokens and the
    // window total match the declared values, provider or not.
    let mut world = world_with_provider();
    let bp = blueprint(vec![leviath_core::Stage::new(
        "main".to_string(),
        leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
    )]);
    let e = spawn_agent(
        &mut world,
        "run".to_string(),
        bp,
        "task",
        vec![resolved("m")],
        true,
    )
    .expect("spawn");
    let w = world.get::<ContextWindow>(e).expect("window");
    // The `blueprint` helper declares total_budget_tokens = 12_000 (legacy sum
    // behavior preserved for absolute layouts).
    assert_eq!(w.max_tokens, 12_000);
    assert_eq!(w.get_region("conversation").unwrap().max_tokens, 10_000);
}

#[test]
fn spawn_agent_seeded_resolves_per_stage_layout() {
    // Stage 0 carries its own percentage layout; it must be resolved against
    // that stage's model window and applied on entry (swapping the global one).
    let mut world = world_with_provider();
    let global = leviath_core::layout::ContextLayout::new(
        vec![leviath_core::layout::RegionDefinition::new(
            "sys".to_string(),
            RegionKind::Pinned,
            5000,
        )],
        5000,
    );
    let mut stage = leviath_core::Stage::new(
        "main".to_string(),
        leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
    );
    stage.context_layout = Some(leviath_core::layout::ContextLayout::new(
        vec![
            leviath_core::layout::RegionDefinition::new("sys".to_string(), RegionKind::Pinned, 0)
                .with_budget(leviath_core::BudgetSpec::Percent {
                    percent: 0.10,
                    min: None,
                    max: None,
                }),
        ],
        0,
    ));
    let bp = leviath_core::Blueprint::new("t".to_string(), "d".to_string(), vec![stage], global);
    let e = spawn_agent(
        &mut world,
        "run".to_string(),
        bp,
        "task",
        vec![resolved("m")],
        true,
    )
    .expect("spawn");
    let w = world.get::<ContextWindow>(e).expect("window");
    // Stage 0's per-stage layout won: 10% of 100_000 = 10_000.
    assert_eq!(w.get_region("sys").unwrap().max_tokens, 10_000);
}

#[test]
fn spawn_agent_seeded_errors_when_resolved_global_layout_is_invalid() {
    // A pinned region at 95% of the 100_000 window resolves to 95_000, leaving
    // only 5_000 working tokens (< MIN_WORKING_TOKENS). Post-resolution
    // validation must fail the spawn with an actionable message.
    let mut world = world_with_provider();
    let err = spawn_agent(
        &mut world,
        "run".to_string(),
        percent_region_blueprint(0.95),
        "task",
        vec![resolved("m")],
        true,
    )
    .expect_err("resolved layout should fail validation");
    assert!(err.contains("working tokens"), "{err}");
}

#[test]
fn spawn_agent_seeded_errors_when_resolved_per_stage_layout_is_invalid() {
    // The global layout is valid, but stage 0's per-stage layout resolves to a
    // starved working budget → the per-stage validation branch fails the spawn.
    let mut world = world_with_provider();
    let global = leviath_core::layout::ContextLayout::new(
        vec![leviath_core::layout::RegionDefinition::new(
            "scratch".to_string(),
            RegionKind::Clearable,
            5000,
        )],
        5000,
    );
    let mut stage = leviath_core::Stage::new(
        "main".to_string(),
        leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
    );
    stage.context_layout = Some(leviath_core::layout::ContextLayout::new(
        vec![
            leviath_core::layout::RegionDefinition::new("sys".to_string(), RegionKind::Pinned, 0)
                .with_budget(leviath_core::BudgetSpec::Percent {
                    percent: 0.95,
                    min: None,
                    max: None,
                }),
        ],
        0,
    ));
    let bp = leviath_core::Blueprint::new("t".to_string(), "d".to_string(), vec![stage], global);
    let err = spawn_agent(
        &mut world,
        "run".to_string(),
        bp,
        "task",
        vec![resolved("m")],
        true,
    )
    .expect_err("per-stage layout should fail validation");
    assert!(err.contains("working tokens"), "{err}");
}

#[test]
fn collect_drops_outcome_for_non_awaiting_agent() {
    let (mut world, tx) = world_with_results();
    let e = world.spawn(agent_state()).id(); // no AwaitingInference marker
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Ok(resp("x")),
    })
    .unwrap();

    run_collect(&mut world);

    // Untouched - the stale outcome was dropped.
    assert_eq!(world.get::<AgentState>(e).unwrap().iteration, 0);
    assert!(world.get::<ProcessResponse>(e).is_none());
}

#[test]
fn collect_inference_accumulates_token_totals() {
    let (mut world, tx) = world_with_results();
    let e = world
        .spawn((
            agent_state(),
            AwaitingInference,
            crate::persistence::TokenTotals::default(),
        ))
        .id();
    let mut r = resp("hi");
    r.tokens_used = leviath_providers::TokenUsage {
        prompt_tokens: 10,
        completion_tokens: 5,
        total_tokens: 15,
        cached_tokens: 2,
        cache_write_tokens: 1,
    };
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Ok(r),
    })
    .unwrap();

    run_collect(&mut world);

    let t = world.get::<crate::persistence::TokenTotals>(e).unwrap();
    assert_eq!(t.prompt_tokens, 10);
    assert_eq!(t.completion_tokens, 5);
    assert_eq!(t.cached_tokens, 2);
    assert_eq!(t.cache_write_tokens, 1);
}

// ── process-response routing ──

/// An inference result, paired with the advertisement that makes its call
/// legal - see [`infer_with`]. `false` yields no calls and so offers
/// nothing, which is what a stage with no tools looks like.
fn infer_result(with_tools: bool) -> (StageInference, crate::components::InferenceResult) {
    let offers = offering(match with_tools {
        true => &["n"],
        false => &[],
    });
    (offers, infer_result_only(with_tools))
}

fn infer_result_only(with_tools: bool) -> crate::components::InferenceResult {
    crate::components::InferenceResult {
        response: "r".to_string(),
        tool_calls: if with_tools {
            vec![crate::components::ToolCall {
                tool_id: "t".to_string(),
                name: "n".to_string(),
                arguments: serde_json::Value::Null,
                thought_signature: None,
            }]
        } else {
            vec![]
        },
        tokens_used: 0,
        timestamp: 0,
    }
}

fn run_process(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(process_response);
    s.run(world);
}

#[test]
fn process_routes_tool_calls_to_ready_for_tools() {
    let mut world = World::new();
    let e = world
        .spawn((
            infer_result(true),
            StageProgress::default(),
            ProcessResponse,
        ))
        .id();
    run_process(&mut world);
    assert!(world.get::<ReadyForTools>(e).is_some());
    assert!(world.get::<ProcessResponse>(e).is_none());
    assert!(world.get::<ReadyForTransition>(e).is_none());
    // The stage's running tool-call count was bumped.
    assert_eq!(world.get::<StageProgress>(e).unwrap().total_tool_calls, 1);
}

#[test]
fn process_response_bumps_tool_calls_in_token_totals() {
    let mut world = World::new();
    let e = world
        .spawn((
            infer_result(true),
            StageProgress::default(),
            crate::persistence::TokenTotals::default(),
            ProcessResponse,
        ))
        .id();
    run_process(&mut world);
    assert_eq!(
        world
            .get::<crate::persistence::TokenTotals>(e)
            .unwrap()
            .tool_calls,
        1
    );
}

/// Per-path churn is counted from the REQUESTED calls, which is what feeds
/// the `stuck_after_same_file_edits` threshold.
#[test]
fn process_response_counts_edits_by_path() {
    let call = |name: &str, path: Option<&str>| crate::components::ToolCall {
        tool_id: "t".to_string(),
        name: name.to_string(),
        arguments: match path {
            Some(p) => serde_json::json!({ "path": p }),
            None => serde_json::Value::Null,
        },
        thought_signature: None,
    };
    let mut world = World::new();
    let e = world
        .spawn((
            crate::components::InferenceResult {
                response: "r".to_string(),
                tool_calls: vec![
                    call("edit_file", Some("where.py")),
                    call("write_file", Some("where.py")),
                    call("edit_file", Some("other.py")),
                    // Neither of these is a mutation of a known path.
                    call("read_file", Some("where.py")),
                    call("bash", None),
                ],
                tokens_used: 0,
                timestamp: 0,
            },
            StageProgress::default(),
            ProcessResponse,
        ))
        .id();
    run_process(&mut world);

    let progress = world.get::<StageProgress>(e).unwrap();
    assert_eq!(progress.edits_by_path.get("where.py"), Some(&2));
    assert_eq!(progress.edits_by_path.get("other.py"), Some(&1));
    assert_eq!(progress.edits_by_path.len(), 2);
    assert_eq!(progress.total_tool_calls, 5);
}

#[test]
fn process_routes_no_tools_to_ready_for_transition() {
    let mut world = World::new();
    let e = world
        .spawn((
            infer_result(false),
            StageProgress::default(),
            ProcessResponse,
        ))
        .id();
    run_process(&mut world);
    assert!(world.get::<ReadyForTransition>(e).is_some());
    assert!(world.get::<ReadyForTools>(e).is_none());
}

// ── empty-response (finish vs. nudge) ──

fn run_empty(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(handle_empty_response);
    s.run(world);
}

/// A one-stage blueprint whose stage either presents its output for review
/// or runs autonomously.
fn nudge_bp(reviewed: bool) -> AgentBlueprint {
    let mut stage = stage_named("a", None, false, None);
    if reviewed {
        let point = leviath_core::blueprint::InteractionPoint {
            name: "plan_approval".to_string(),
            prompt: "Review the plan above.".to_string(),
            required: true,
            style: leviath_core::blueprint::InteractionStyle::MultipleChoice,
            options: vec!["Approve".to_string()],
            directives: std::collections::HashMap::new(),
            abort_options: Vec::new(),
            edit_options: Vec::new(),
            document_region: Some("plan".to_string()),
        };
        stage.mode = leviath_core::blueprint::StageMode::InteractivePoints {
            points: vec![point],
        };
    }
    AgentBlueprint(blueprint(vec![stage]))
}

#[test]
fn empty_response_finishes_when_agent_made_tool_calls() {
    let mut world = World::new();
    let progress = StageProgress {
        total_tool_calls: 2,
        text_only_nudges: 0,
        iterations: 0,
        ..Default::default()
    };
    let e = world
        .spawn((
            ctx(&[("conversation", 10_000)]),
            infer_result(false),
            progress,
            nudge_bp(false),
            StageCursor { index: 0 },
            ReadyForTransition,
        ))
        .id();
    run_empty(&mut world);
    assert!(world.get::<ResolveTransition>(e).is_some());
    assert!(world.get::<ReadyForTransition>(e).is_none());
}

#[test]
fn empty_response_finishes_after_max_nudges() {
    let mut world = World::new();
    let progress = StageProgress {
        total_tool_calls: 0,
        text_only_nudges: MAX_TEXT_ONLY_NUDGES,
        iterations: 0,
        ..Default::default()
    };
    let e = world
        .spawn((
            ctx(&[("conversation", 10_000)]),
            infer_result(false),
            progress,
            nudge_bp(false),
            StageCursor { index: 0 },
            ReadyForTransition,
        ))
        .id();
    run_empty(&mut world);
    assert!(world.get::<ResolveTransition>(e).is_some());
}

/// A stage that presents its output for review is finished when it produces
/// that output. This is the whole failure, from a real run: `plan` wrote a
/// complete plan on its first turn - correctly, with no tool calls, because
/// writing the plan *is* the job - and the nudge read that as a model
/// stalling and told it to "use your tools to complete the task". `plan`
/// has no tool that writes anything, so the model went looking for one,
/// could not find it, and asked the user to grant it a write tool or create
/// the file by hand. The plan it had already finished was never presented.
#[test]
fn empty_response_never_nudges_a_stage_whose_output_is_reviewed() {
    let mut world = World::new();
    let e = world
        .spawn((
            ctx(&[("conversation", 10_000)]),
            infer_result(false),      // text only, no tool calls
            StageProgress::default(), // and no work done yet this stage
            nudge_bp(true),
            StageCursor { index: 0 },
            ReadyForTransition,
        ))
        .id();
    run_empty(&mut world);

    assert!(
        world.get::<ResolveTransition>(e).is_some(),
        "the stage is done: its text is what gets reviewed"
    );
    assert!(
        world.get::<ReadyToInfer>(e).is_none(),
        "not sent round again"
    );
    assert_eq!(
        world.get::<StageProgress>(e).unwrap().text_only_nudges,
        0,
        "and not counted as a nudge"
    );
    // Nothing was injected - the model is not told to go do work it has no
    // tool for, which is what sent it asking the user for one.
    assert!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .content
            .is_empty(),
        "nothing is injected: no nudge telling the model to go do work it \
         has no tool for, which is what sent it asking the user for one"
    );
}

#[test]
fn empty_response_nudges_and_loops_back_when_text_only() {
    let mut world = World::new();
    let e = world
        .spawn((
            ctx(&[("conversation", 10_000)]),
            infer_result(false),
            StageProgress::default(),
            nudge_bp(false),
            StageCursor { index: 0 },
            ReadyForTransition,
        ))
        .id();
    run_empty(&mut world);
    // Nudged: back to infer, counter bumped, nudge added to context.
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<ResolveTransition>(e).is_none());
    assert_eq!(world.get::<StageProgress>(e).unwrap().text_only_nudges, 1);
    assert!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .current_tokens
            > 0
    );
}

// ── tool-dispatch ──

/// A tool service that echoes each call as `(id, "ran <name>")`.
struct EchoService;
impl ToolService for EchoService {
    fn exec_for(&self, _entity: Entity, calls: Vec<leviath_providers::ToolCall>) -> BoxedToolExec {
        Box::new(move || {
            Box::pin(async move {
                calls
                    .into_iter()
                    .map(|c| (c.id, format!("ran {}", c.name)))
                    .collect()
            })
        })
    }
}

/// A tool service that records every `sync_stage` call.
#[derive(Default)]
struct RecordingService(Arc<std::sync::Mutex<Vec<(Entity, usize, String)>>>);
impl ToolService for RecordingService {
    fn exec_for(&self, _entity: Entity, _calls: Vec<leviath_providers::ToolCall>) -> BoxedToolExec {
        Box::new(|| Box::pin(async { Vec::new() }))
    }
    fn sync_stage(&self, entity: Entity, stage_index: usize, stage_name: &str) {
        self.0
            .lock()
            .unwrap()
            .push((entity, stage_index, stage_name.to_string()));
    }
}

#[tokio::test]
async fn sync_tool_stages_notifies_service_and_clears_marker() {
    let mut world = World::new();
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let service = Arc::new(RecordingService(log.clone()));
    world.insert_resource(ToolServiceRes(service.clone()));
    let entity = world
        .spawn(StageJustEntered {
            index: 2,
            name: "review".to_string(),
        })
        .id();
    let mut schedule = Schedule::default();
    schedule.add_systems(sync_tool_stages);
    schedule.run(&mut world);

    assert_eq!(
        log.lock().unwrap().as_slice(),
        &[(entity, 2, "review".to_string())]
    );
    // The transient marker is cleared after notifying.
    assert!(world.get::<StageJustEntered>(entity).is_none());
    // The service's tool executor still runs (returns no results here).
    assert!(service.exec_for(entity, Vec::new())().await.is_empty());
}

#[test]
fn default_sync_stage_is_a_noop() {
    // A service that doesn't override `sync_stage` uses the no-op default.
    EchoService.sync_stage(
        Entity::from_raw_u32(0).expect("a small literal index is always a valid entity id"),
        3,
        "x",
    );
}

#[tokio::test]
async fn default_refresh_tools_returns_none() {
    // A service that doesn't override `refresh_tools` uses the None default.
    assert!(
        EchoService
            .refresh_tools(
                Entity::from_raw_u32(0).expect("a small literal index is always a valid entity id"),
                0
            )
            .is_none()
    );
    // Exercise RefreshService's (unused-by-the-system) exec_for closure too.
    assert!(
        RefreshService(vec![]).exec_for(
            Entity::from_raw_u32(0).expect("a small literal index is always a valid entity id"),
            Vec::new()
        )()
        .await
        .is_empty()
    );
}

/// A service whose `refresh_tools` returns a fixed set of tool names.
struct RefreshService(Vec<&'static str>);
impl ToolService for RefreshService {
    fn exec_for(&self, _e: Entity, _c: Vec<leviath_providers::ToolCall>) -> BoxedToolExec {
        Box::new(|| Box::pin(async { Vec::new() }))
    }
    fn refresh_tools(&self, _e: Entity, _idx: usize) -> Option<Vec<Tool>> {
        Some(
            self.0
                .iter()
                .map(|n| Tool {
                    name: n.to_string(),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                })
                .collect(),
        )
    }
}

fn stage_inf(tools: &[&str]) -> StageInference {
    StageInference {
        provider_name: "p".to_string(),
        model: "m".to_string(),
        tools: tools
            .iter()
            .map(|n| Tool {
                name: n.to_string(),
                description: String::new(),
                parameters: serde_json::json!({}),
            })
            .collect(),
        tool_filter: None,
    }
}

fn run_refresh(world: &mut World) {
    let mut schedule = Schedule::default();
    schedule.add_systems(refresh_advertised_tools);
    schedule.run(world);
}

#[test]
fn refresh_advertised_tools_updates_live_and_catalog() {
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(RefreshService(vec!["new_tool"]))));
    let entity = world
        .spawn((
            StageCursor { index: 0 },
            stage_inf(&["old"]),
            StageInferences(vec![stage_inf(&["old"]), stage_inf(&["other"])]),
            ToolsNeedRefresh,
        ))
        .id();
    run_refresh(&mut world);

    // Live component + the current catalog entry now advertise the new tool.
    let names: Vec<String> = world
        .get::<StageInference>(entity)
        .unwrap()
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert_eq!(names, vec!["new_tool".to_string()]);
    let cat0: Vec<String> = world.get::<StageInferences>(entity).unwrap().0[0]
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert_eq!(cat0, vec!["new_tool".to_string()]);
    // Other stages in the catalog are untouched.
    assert_eq!(
        world.get::<StageInferences>(entity).unwrap().0[1].tools[0].name,
        "other"
    );
    // Marker consumed.
    assert!(world.get::<ToolsNeedRefresh>(entity).is_none());
}

#[test]
fn refresh_advertised_tools_none_leaves_tools_but_clears_marker() {
    // EchoService::refresh_tools returns None → the advertised set is unchanged
    // but the marker is still consumed (no busy re-tagging).
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    let entity = world
        .spawn((
            StageCursor { index: 0 },
            stage_inf(&["keep"]),
            StageInferences(vec![stage_inf(&["keep"])]),
            ToolsNeedRefresh,
        ))
        .id();
    run_refresh(&mut world);
    assert_eq!(
        world.get::<StageInference>(entity).unwrap().tools[0].name,
        "keep"
    );
    assert!(world.get::<ToolsNeedRefresh>(entity).is_none());
}

/// A service whose `wants_refresh` returns a fixed value.
struct PollService(bool);
impl ToolService for PollService {
    fn exec_for(&self, _e: Entity, _c: Vec<leviath_providers::ToolCall>) -> BoxedToolExec {
        Box::new(|| Box::pin(async { Vec::new() }))
    }
    fn wants_refresh(&self, _e: Entity) -> bool {
        self.0
    }
}

#[tokio::test]
async fn default_wants_refresh_returns_false() {
    assert!(!EchoService.wants_refresh(
        Entity::from_raw_u32(0).expect("a small literal index is always a valid entity id")
    ));
    // Exercise PollService's (unused-by-the-system) exec_for closure.
    assert!(
        PollService(false).exec_for(
            Entity::from_raw_u32(0).expect("a small literal index is always a valid entity id"),
            Vec::new()
        )()
        .await
        .is_empty()
    );
}

fn run_poll(world: &mut World) {
    let mut schedule = Schedule::default();
    schedule.add_systems(poll_dynamic_tool_refresh);
    schedule.run(world);
}

#[test]
fn poll_tags_dynamic_agent_when_service_wants_refresh() {
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(PollService(true))));
    let dyn_e = world.spawn(DynamicTools).id();
    // A non-dynamic agent is never polled, even if the service wants refresh.
    let static_e = world.spawn_empty().id();
    run_poll(&mut world);
    assert!(world.get::<ToolsNeedRefresh>(dyn_e).is_some());
    assert!(world.get::<ToolsNeedRefresh>(static_e).is_none());
}

#[test]
fn poll_leaves_dynamic_agent_untagged_when_no_refresh_wanted() {
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(PollService(false))));
    let dyn_e = world.spawn(DynamicTools).id();
    run_poll(&mut world);
    assert!(world.get::<ToolsNeedRefresh>(dyn_e).is_none());
}

#[test]
fn refresh_advertised_tools_tolerates_cursor_past_catalog() {
    // A cursor index beyond the catalog updates only the live component
    // (the `get_mut(index)` None arm), never panicking.
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(RefreshService(vec!["fresh"]))));
    let entity = world
        .spawn((
            StageCursor { index: 5 },
            stage_inf(&["old"]),
            StageInferences(vec![stage_inf(&["old"])]),
            ToolsNeedRefresh,
        ))
        .id();
    run_refresh(&mut world);
    assert_eq!(
        world.get::<StageInference>(entity).unwrap().tools[0].name,
        "fresh"
    );
    // The single catalog entry is untouched (index 5 doesn't exist).
    assert_eq!(
        world.get::<StageInferences>(entity).unwrap().0[0].tools[0].name,
        "old"
    );
}

#[tokio::test]
async fn dispatch_tools_enqueues_runnable_job_and_advances() {
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    let e = world
        .spawn((
            agent_state(),
            infer_result(true),
            conv_window(),
            ReadyForTools,
        ))
        .id();

    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    assert!(world.get::<AwaitingTools>(e).is_some());
    assert!(world.get::<ReadyForTools>(e).is_none());
    let job = jrx.try_recv().expect("job enqueued");
    assert_eq!(job.entity, e);
    // Run the produced closure (covers the service's exec path).
    let results = (job.exec)().await;
    assert_eq!(results, vec![("t".to_string(), "ran n".to_string())]);
}

#[tokio::test]
async fn dispatch_tools_skips_non_active_agent() {
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    let mut st = agent_state();
    st.status = AgentStatus::Cancelled;
    let e = world
        .spawn((st, infer_result(true), conv_window(), ReadyForTools))
        .id();

    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    assert!(world.get::<ReadyForTools>(e).is_some()); // cancelled ⇒ not enqueued
    assert!(jrx.try_recv().is_err());
}

/// A stage advertising exactly `names`.
fn offering(names: &[&str]) -> StageInference {
    StageInference {
        provider_name: "p".to_string(),
        model: "m".to_string(),
        tools: names
            .iter()
            .map(|n| leviath_providers::Tool {
                name: (*n).to_string(),
                description: String::new(),
                parameters: serde_json::json!({}),
            })
            .collect(),
        tool_filter: None,
    }
}

/// An inference result **and** the advertisement that makes its own calls
/// legal - dispatch refuses a tool the stage never offered, so a fixture
/// that calls one has to offer it. Returned together as a bundle so every
/// test exercising some *other* part of dispatch is not restating its own
/// call list. Tests about the Layer-1 check itself build the two separately.
fn infer_with(
    calls: Vec<crate::components::ToolCall>,
) -> (StageInference, crate::components::InferenceResult) {
    let offers = offering(&calls.iter().map(|c| c.name.as_str()).collect::<Vec<_>>());
    (
        offers,
        crate::components::InferenceResult {
            response: "r".to_string(),
            tool_calls: calls,
            tokens_used: 0,
            timestamp: 0,
        },
    )
}

fn ctx_call(id: &str, region: &str, content: &str) -> crate::components::ToolCall {
    crate::components::ToolCall {
        tool_id: id.to_string(),
        name: "context_write".to_string(),
        arguments: serde_json::json!({"region": region, "content": content}),
        thought_signature: None,
    }
}

fn notes_window() -> ContextWindow {
    let mut w = conv_window();
    w.add_region(Region::new(
        "notes".to_string(),
        RegionKind::Clearable,
        5000,
    ));
    w
}

#[tokio::test]
async fn dispatch_tools_applies_all_context_inline() {
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    let e = world
        .spawn((
            agent_state(),
            infer_with(vec![ctx_call("c1", "notes", "hi")]),
            notes_window(),
            ReadyForTools,
        ))
        .id();

    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    // All-context batch: nothing enqueued, applied inline, ready to infer.
    assert!(jrx.try_recv().is_err());
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<ReadyForTools>(e).is_none());
    assert!(world.get::<ContextToolResults>(e).is_none());
    assert!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("notes")
            .unwrap()
            .current_tokens
            > 0
    );
}

/// The text dispatch left in the agent's conversation for the model to read.
/// A batch with no lane work is applied inline, so there is no
/// `ContextToolResults` to inspect - the window is the only record.
fn conversation_text(world: &World, e: Entity) -> String {
    world
        .get::<ContextWindow>(e)
        .unwrap()
        .get_region("conversation")
        .unwrap()
        .content
        .iter()
        .map(|entry| entry.content_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The reason this check exists, as it actually happened: a `plan` stage
/// granting only reads emitted `write_file` with a complete source file in
/// it. `available_tools` was applied when building the schema list and never
/// again, so the call was dispatched anyway and the *user* was asked to
/// approve writing code from the planning stage. It never reaches the lane
/// or the permission gate now - the model is told, and the turn continues.
#[tokio::test]
async fn dispatch_tools_refuses_a_tool_the_stage_never_offered() {
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    let (_, result) = infer_with(vec![tc("c1", "write_file"), tc("c2", "read_file")]);
    let e = world
        .spawn((
            agent_state(),
            offering(&["read_file", "list_dir"]),
            result,
            conv_window(),
            ReadyForTools,
        ))
        .id();

    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    let stashed = &world.get::<ContextToolResults>(e).unwrap().0;
    assert_eq!(
        stashed.len(),
        1,
        "only the unoffered call was answered here"
    );
    assert_eq!(stashed[0].0, "c1");
    let refusal = stashed[0].1.clone();
    assert!(refusal.contains("not available in this stage"), "{refusal}");
    // And it names what the model *can* use, so the next turn is a usable
    // call rather than a retry of the same one.
    assert!(refusal.contains("read_file"), "{refusal}");

    // The offered call still went to the lane: this refuses what was not
    // granted, it does not refuse everything.
    let job = jrx.try_recv().expect("the offered call still runs");
    assert_eq!(job.entity, e);
}

/// A stage may advertise nothing at all (`available_tools = []` is a real
/// setting, not "unset"). Saying "you may call: " with an empty list would
/// read as a bug, so it says what is true instead.
#[tokio::test]
async fn dispatch_tools_tells_a_toolless_stage_to_answer_directly() {
    let (jtx, _jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    let (_, result) = infer_with(vec![tc("c1", "read_file")]);
    let e = world
        .spawn((
            agent_state(),
            offering(&[]),
            result,
            conv_window(),
            ReadyForTools,
        ))
        .id();

    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    let text = conversation_text(&world, e);
    assert!(
        text.contains("no tools at all") && text.contains("Answer directly"),
        "{text}"
    );
}

/// Aliases resolve on both sides. A manifest says `bash` and the model calls
/// `shell` (or the reverse) - matching the raw strings would refuse a tool
/// the stage plainly granted, which is a worse failure than the one this
/// check exists to prevent.
#[tokio::test]
async fn dispatch_tools_matches_an_offered_tool_through_its_alias() {
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    let canonical = leviath_tools::canonical_tool_name("bash");
    assert_ne!(
        canonical, "bash",
        "this test needs a real alias to be a test"
    );
    let (_, result) = infer_with(vec![tc("c1", canonical)]);
    let e = world
        .spawn((
            agent_state(),
            offering(&["bash"]),
            result,
            conv_window(),
            ReadyForTools,
        ))
        .id();

    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    assert!(
        world.get::<ContextToolResults>(e).unwrap().0.is_empty(),
        "nothing was refused"
    );
    assert!(jrx.try_recv().is_ok(), "the aliased call ran");
}

/// `tool_filter` narrows what a request advertises, so it has to narrow what
/// dispatch accepts too - otherwise the filtered-out tool is callable by
/// name, which is the exact hole this check closes one level up.
#[tokio::test]
async fn dispatch_tools_honours_the_stage_tool_filter() {
    let (jtx, _jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    let mut offers = offering(&["read_file", "write_file"]);
    offers.tool_filter = Some(vec!["read_file".to_string()]);
    let (_, result) = infer_with(vec![tc("c1", "write_file")]);
    let e = world
        .spawn((agent_state(), offers, result, conv_window(), ReadyForTools))
        .id();

    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    let text = conversation_text(&world, e);
    assert!(text.contains("not available in this stage"), "{text}");
}

/// An empty `tool_filter` means "no narrowing", matching the request
/// builder - not "nothing is allowed".
#[tokio::test]
async fn dispatch_tools_treats_an_empty_tool_filter_as_no_narrowing() {
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    let mut offers = offering(&["read_file"]);
    offers.tool_filter = Some(vec![]);
    let (_, result) = infer_with(vec![tc("c1", "read_file")]);
    let e = world
        .spawn((agent_state(), offers, result, conv_window(), ReadyForTools))
        .id();

    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    assert!(
        world.get::<ContextToolResults>(e).unwrap().0.is_empty(),
        "nothing was refused"
    );
    assert!(jrx.try_recv().is_ok(), "the call ran");
}

/// Context tools go through the same gate. They are applied inline rather
/// than on the lane, so a check that lived only in the lane would have left
/// `context_write` callable from a stage that never granted it.
#[tokio::test]
async fn dispatch_tools_refuses_an_unoffered_context_tool() {
    let (jtx, _jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    let (_, result) = infer_with(vec![ctx_call("c1", "notes", "smuggled")]);
    let e = world
        .spawn((
            agent_state(),
            offering(&["read_file"]),
            result,
            notes_window(),
            ReadyForTools,
        ))
        .id();

    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    let text = conversation_text(&world, e);
    assert!(text.contains("not available in this stage"), "{text}");
    // And nothing was written to the region.
    let w = world.get::<ContextWindow>(e).unwrap();
    assert!(w.get_region("notes").unwrap().content.is_empty());
}

#[tokio::test]
async fn dispatch_tools_partitions_context_and_lane() {
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    let e = world
        .spawn((
            agent_state(),
            infer_with(vec![ctx_call("c1", "notes", "hi"), tc("c2", "read_file")]),
            notes_window(),
            ReadyForTools,
        ))
        .id();

    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    // Context result stashed; the non-context call went to the lane.
    assert!(world.get::<AwaitingTools>(e).is_some());
    let stashed = world.get::<ContextToolResults>(e).unwrap();
    assert_eq!(stashed.0.len(), 1);
    assert_eq!(stashed.0[0].0, "c1");
    let job = jrx.try_recv().expect("lane job for the non-context call");
    assert_eq!(job.entity, e);
}

// ── taint gate (dispatch_tools) ──

/// A taint-tracking window carrying `Internal`-level data.
fn tainted_conv_window() -> ContextWindow {
    let mut w = conv_window();
    w.enable_taint_tracking();
    let _ = w.add_typed_tainted_to_region(
        "conversation",
        leviath_core::EntryKind::UserMessage,
        "secret".to_string(),
        5,
        leviath_core::TaintLevel::Internal,
    );
    w
}

fn enabled_gate() -> crate::taint::TaintGate {
    crate::taint::TaintGate::new(leviath_core::SecurityConfig {
        taint_tracking: true,
    })
}

#[tokio::test]
async fn dispatch_tools_gate_blocks_outbound_leak_but_allows_inbound() {
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    // `shell` is outbound (clearance Public) over Internal data ⇒ blocked;
    // `read_file` is inbound ⇒ always allowed ⇒ goes to the lane.
    let e = world
        .spawn((
            agent_state(),
            infer_with(vec![tc("c_shell", "shell"), tc("c_read", "read_file")]),
            tainted_conv_window(),
            ReadyForTools,
            enabled_gate(),
        ))
        .id();
    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    assert!(world.get::<AwaitingTools>(e).is_some());
    let stashed = world.get::<ContextToolResults>(e).unwrap();
    assert!(
        stashed
            .0
            .iter()
            .any(|(id, msg)| id == "c_shell" && msg.contains("[blocked]"))
    );
    let job = jrx.try_recv().expect("read_file enqueued to the lane");
    assert_eq!(job.entity, e);
}

#[tokio::test]
async fn dispatch_tools_holds_batch_for_an_interactive_gate_prompt() {
    let (jtx, _jrx) = mpsc::unbounded_channel();
    let (gtx, _grx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(std::sync::Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    world.insert_resource(crate::interaction_hub::InteractionHub::new());
    world.insert_resource(crate::gate_prompt::GatePromptStage {
        outcomes: gtx,
        wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        runtime: tokio::runtime::Handle::current(),
    });
    let e = world
        .spawn((
            agent_state(),
            infer_with(vec![tc("c_shell", "shell")]),
            tainted_conv_window(),
            ReadyForTools,
            enabled_gate(),
        ))
        .id();
    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);
    // Blocked + interactive ⇒ held for a prompt, not dispatched or [blocked].
    assert_eq!(
        world
            .get::<crate::gate_prompt::AwaitingGatePrompt>(e)
            .unwrap()
            .0,
        1
    );
    assert!(world.get::<crate::gate_prompt::GateResolved>(e).is_some());
    assert!(world.get::<ReadyForTools>(e).is_none());
    assert!(world.get::<AwaitingTools>(e).is_none());
}

#[tokio::test]
async fn dispatch_tools_auto_approves_a_gate_block_under_yolo() {
    // Same blocked + interactive scenario as above, but the agent carries
    // `GateAutoApprove` (set by `--yolo`): the gate is waived, so the call
    // dispatches to the lane instead of raising a prompt no one can answer.
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let (gtx, _grx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(std::sync::Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    world.insert_resource(crate::interaction_hub::InteractionHub::new());
    world.insert_resource(crate::gate_prompt::GatePromptStage {
        outcomes: gtx,
        wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        runtime: tokio::runtime::Handle::current(),
    });
    let e = world
        .spawn((
            agent_state(),
            infer_with(vec![tc("c_shell", "shell")]),
            tainted_conv_window(),
            ReadyForTools,
            enabled_gate(),
            crate::components::GateAutoApprove,
        ))
        .id();
    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);
    // No gate prompt was raised; the call went to the lane.
    assert!(
        world
            .get::<crate::gate_prompt::AwaitingGatePrompt>(e)
            .is_none()
    );
    assert!(world.get::<AwaitingTools>(e).is_some());
    assert!(world.get::<ReadyForTools>(e).is_none());
    assert_eq!(jrx.try_recv().expect("job enqueued").entity, e);
    // The waived block is still recorded in the audit trail. Evaluate the
    // predicate first so the assert message stays static (a call in the
    // message only runs on failure and would read as uncovered).
    let recorded_yolo_override = world
        .get::<crate::taint::TaintGate>(e)
        .unwrap()
        .audit_log()
        .iter()
        .any(|ev| {
            ev.allowed
                && ev.decision_source == leviath_core::taint::GateDecisionSource::YoloAutoApprove
        });
    assert!(
        recorded_yolo_override,
        "expected a YoloAutoApprove audit entry"
    );
}

#[tokio::test]
async fn dispatch_tools_executes_a_gate_approved_call_and_blocks_a_denied_one() {
    // approved ⇒ reaches the lane; denied ⇒ its stored message, no lane call.
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(std::sync::Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    let mut resolved = crate::gate_prompt::GateResolved::default();
    resolved.approved.insert("c_ok".to_string());
    resolved
        .denied
        .insert("c_no".to_string(), "[blocked] user denied".to_string());
    let e = world
        .spawn((
            agent_state(),
            infer_with(vec![tc("c_ok", "shell"), tc("c_no", "shell")]),
            tainted_conv_window(),
            ReadyForTools,
            enabled_gate(),
            resolved,
        ))
        .id();
    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    // The approved call was enqueued to the lane; the denied one was not.
    let job = jrx.try_recv().expect("approved call enqueued");
    assert_eq!(job.entity, e);
    assert!(world.get::<AwaitingTools>(e).is_some());
    // The denied message is stashed for merge with the lane results.
    let stashed = world.get::<ContextToolResults>(e).unwrap();
    assert!(
        stashed
            .0
            .iter()
            .any(|(id, msg)| id == "c_no" && msg.contains("user denied"))
    );
    // The resolution state was consumed.
    assert!(world.get::<crate::gate_prompt::GateResolved>(e).is_none());
}

#[tokio::test]
async fn dispatch_tools_falls_through_for_a_resolved_agents_unprompted_call() {
    // An agent still carrying GateResolved, with a call that is in neither
    // `approved` nor `denied` (it was allowed on the first pass and never
    // prompted), falls through the resolution bypass to the normal gate
    // check - which allows the inbound `read_file` and sends it to the lane.
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    let e = world
        .spawn((
            agent_state(),
            infer_with(vec![tc("c_read", "read_file")]),
            tainted_conv_window(),
            ReadyForTools,
            enabled_gate(),
            crate::gate_prompt::GateResolved::default(),
        ))
        .id();
    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);
    // Inbound read_file is gate-allowed ⇒ reaches the lane.
    let job = jrx.try_recv().expect("allowed call enqueued");
    assert_eq!(job.entity, e);
    // GateResolved is consumed once the batch dispatches.
    assert!(world.get::<crate::gate_prompt::GateResolved>(e).is_none());
}

#[tokio::test]
async fn dispatch_tools_gate_allows_outbound_via_allowlist() {
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    // An allowlist rule permits `shell` up to Internal sensitivity.
    world.insert_resource(PolicyGate(leviath_core::PolicyConfig {
        allowlist: vec![leviath_core::policy::AllowlistRule {
            tool: "shell".to_string(),
            to: vec![],
            channel: vec![],
            max_sensitivity: leviath_core::TaintLevel::Internal,
        }],
        mcp_overrides: Default::default(),
    }));
    let e = world
        .spawn((
            agent_state(),
            infer_with(vec![tc("c_shell", "shell")]),
            tainted_conv_window(),
            ReadyForTools,
            enabled_gate(),
        ))
        .id();
    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    // Allowlisted ⇒ the outbound call reaches the lane instead of `[blocked]`.
    assert!(world.get::<AwaitingTools>(e).is_some());
    let job = jrx.try_recv().expect("shell enqueued via allowlist");
    assert_eq!(job.entity, e);
}

#[tokio::test]
async fn dispatch_tools_gate_allows_outbound_via_scripted_rule() {
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage(jtx));
    // No static allowlist, but a scripted rule that permits `shell`.
    let checker: std::sync::Arc<crate::taint::ScriptRuleChecker> =
        std::sync::Arc::new(|tool: &str, _target: Option<&str>, _taint| {
            (tool == "shell").then(|| "scripted".to_string())
        });
    world.insert_resource(GateScriptRules(checker));
    let e = world
        .spawn((
            agent_state(),
            infer_with(vec![tc("c_shell", "shell")]),
            tainted_conv_window(),
            ReadyForTools,
            enabled_gate(),
        ))
        .id();
    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    // The scripted rule allows it ⇒ reaches the lane, not `[blocked]`.
    assert!(world.get::<AwaitingTools>(e).is_some());
    let job = jrx.try_recv().expect("shell enqueued via scripted rule");
    assert_eq!(job.entity, e);
}

#[test]
fn taint_block_message_renders_blocked_and_falls_back() {
    use leviath_core::taint::GateDecision;
    let blocked = GateDecision::Blocked {
        taint_level: leviath_core::TaintLevel::Internal,
        clearance: leviath_core::TaintLevel::Public,
        source_regions: vec!["conversation".to_string()],
        tool_name: "shell".to_string(),
    };
    let msg = taint_block_message(&blocked);
    assert!(msg.contains("shell") && msg.contains("conversation") && msg.contains("[blocked]"));
    // Empty source regions render as "context".
    let blocked_empty = GateDecision::Blocked {
        taint_level: leviath_core::TaintLevel::Internal,
        clearance: leviath_core::TaintLevel::Public,
        source_regions: vec![],
        tool_name: "shell".to_string(),
    };
    assert!(taint_block_message(&blocked_empty).contains("context"));
    // The Allowed arm is only a defensive fallback.
    assert!(taint_block_message(&GateDecision::Allowed).contains("blocked"));
}

#[test]
fn merge_in_call_order_fills_missing_with_empty() {
    let calls = vec![tc("a", "x"), tc("b", "y")];
    // Only "a" has a result; "b" falls back to empty, in call order.
    let merged = merge_in_call_order(&calls, &[("a".to_string(), "ra".to_string())]);
    assert_eq!(
        merged,
        vec![
            ("a".to_string(), "ra".to_string()),
            ("b".to_string(), String::new()),
        ]
    );
}

// ── tool-collect (apply_tool_results) ──

fn ctx(regions: &[(&str, usize)]) -> ContextWindow {
    let mut w = ContextWindow::new(100_000);
    for (name, max) in regions {
        w.add_region(Region::new(name.to_string(), RegionKind::Clearable, *max));
    }
    w
}

fn tc(id: &str, name: &str) -> crate::components::ToolCall {
    crate::components::ToolCall {
        tool_id: id.to_string(),
        name: name.to_string(),
        arguments: serde_json::Value::Null,
        thought_signature: None,
    }
}

fn routing(
    default: &str,
    overrides: &[(&str, &str)],
    persist: bool,
    max_result: Option<usize>,
) -> leviath_core::blueprint::ToolResultRouting {
    leviath_core::blueprint::ToolResultRouting {
        default_region: default.to_string(),
        tool_overrides: overrides
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        persist,
        max_result_tokens: max_result,
    }
}

#[test]
fn apply_adds_assistant_turn_and_result_to_conversation() {
    let mut w = ctx(&[("conversation", 10_000)]);
    apply_tool_results(
        &mut w,
        "resp",
        &[tc("c1", "read")],
        &[("c1".to_string(), "result".to_string())],
        None,
        None,
    );
    assert!(w.get_region("conversation").unwrap().current_tokens > 0);
}

#[test]
fn thought_signature_survives_the_full_context_round_trip() {
    // The whole reason the field exists: capture -> persist in the
    // conversation region -> reappear on the assembled ToolUse block, so the
    // next request can replay it to a provider (Gemini) that requires it.
    // A Sliding region, because only conversation-shaped regions assemble
    // into messages (Clearable content becomes system text).
    let mut w = ContextWindow::new(100_000);
    w.add_region(Region::new(
        "conversation".to_string(),
        RegionKind::SlidingWindow {
            max_items: 20,
            eviction_strategy: leviath_core::EvictionStrategy::PerItem,
        },
        10_000,
    ));
    let call = crate::components::ToolCall {
        tool_id: "c1".to_string(),
        name: "read".to_string(),
        arguments: serde_json::json!({}),
        thought_signature: Some("sig-bytes".to_string()),
    };
    apply_tool_results(
        &mut w,
        "resp",
        &[call],
        &[("c1".to_string(), "result".to_string())],
        None,
        None,
    );
    let assembled = w.assemble();
    let sigs: Vec<Option<&str>> = assembled
        .messages
        .iter()
        .filter_map(|m| match &m.content {
            leviath_providers::MessageContent::Blocks(blocks) => Some(blocks),
            leviath_providers::MessageContent::Text(_) => None,
        })
        .flatten()
        .filter_map(|b| match b {
            leviath_providers::ContentBlock::ToolUse {
                thought_signature, ..
            } => Some(thought_signature.as_deref()),
            _ => None,
        })
        .collect();
    assert_eq!(
        sigs,
        vec![Some("sig-bytes")],
        "the signature must reach the assembled request"
    );
}

#[test]
fn apply_falls_back_when_region_missing() {
    let mut w = ctx(&[]); // no "conversation" region - every add errors
    // Exhausts the forced-add fallback to the placeholder without panicking.
    apply_tool_results(
        &mut w,
        "resp",
        &[tc("c1", "read")],
        &[("c1".to_string(), "long result".to_string())],
        None,
        None,
    );
}

#[test]
fn apply_routes_to_override_region() {
    let mut w = ctx(&[("conversation", 10_000), ("special", 10_000)]);
    let r = routing("conversation", &[("read", "special")], true, None);
    apply_tool_results(
        &mut w,
        "resp",
        &[tc("c1", "read")],
        &[("c1".to_string(), "x".to_string())],
        Some(&r),
        None,
    );
    assert!(w.get_region("special").unwrap().current_tokens > 0);
}

#[test]
fn routing_away_pointer_previews_and_truncates_long_results() {
    // A routed result longer than the 160-char preview gets an ellipsis in the
    // conversation pointer; the full text still lands in the region.
    let mut w = ctx(&[("conversation", 10_000), ("codebase", 10_000)]);
    let long = "L".repeat(500);
    let r = routing("conversation", &[("read_file", "codebase")], true, None);
    apply_tool_results(
        &mut w,
        "read",
        &[tc("c1", "read_file")],
        &[("c1".to_string(), long.clone())],
        Some(&r),
        None,
    );
    let conv_txt: String = w
        .get_region("conversation")
        .unwrap()
        .content
        .iter()
        .map(|e| e.content_owned())
        .collect();
    assert!(
        conv_txt.contains('…'),
        "long result pointer should be elided"
    );
    assert!(
        w.get_region("codebase")
            .unwrap()
            .content
            .iter()
            .any(|e| e.content().contains(&long)),
        "full result stored in the region"
    );
}

#[test]
fn routing_away_keeps_pair_in_conversation_and_text_in_region() {
    // Regression: routing a tool result to a knowledge region must keep the
    // tool_use/tool_result PAIR in `conversation` (a pointer) and store the full
    // output in the region as TEXT - so assemble() produces a valid, orphan-free
    // message sequence (no ToolResult block outside conversation → no API 400;
    // no orphaned tool_use → no write-loop).
    let mut w = ContextWindow::new(100_000);
    w.add_region(Region::new(
        "codebase".to_string(),
        RegionKind::Temporary,
        10_000,
    ));
    w.add_region(Region::new(
        "conversation".to_string(),
        RegionKind::SlidingWindow {
            max_items: 100,
            eviction_strategy: leviath_core::EvictionStrategy::PerItem,
        },
        10_000,
    ));
    // A plain user message renders as a non-Blocks message (exercises the
    // other arm of the assemble scan below).
    w.add_typed_entry(
        "conversation",
        leviath_core::EntryKind::UserMessage,
        "please read a.rs".to_string(),
        5,
    )
    .unwrap();
    let r = routing("conversation", &[("read_file", "codebase")], true, None);
    apply_tool_results(
        &mut w,
        "I'll read it.",
        &[tc("c1", "read_file")],
        &[("c1".to_string(), "FULL FILE BODY".to_string())],
        Some(&r),
        None,
    );

    // Full output landed in the knowledge region as text.
    let cb = w.get_region("codebase").unwrap();
    assert!(
        cb.content
            .iter()
            .any(|e| e.content().contains("FULL FILE BODY"))
    );
    assert!(
        cb.content
            .iter()
            .all(|e| matches!(e.kind, leviath_core::EntryKind::Text)),
        "routed content must be stored as Text, not a ToolResult block"
    );

    // Conversation holds the tool_use AND a paired tool_result (pointer).
    let conv = w.get_region("conversation").unwrap();
    assert!(conv.content.iter().any(
        |e| matches!(&e.kind, leviath_core::EntryKind::AssistantTurn { tool_calls } if tool_calls.iter().any(|c| c.id == "c1"))
    ));
    assert!(conv.content.iter().any(
        |e| matches!(&e.kind, leviath_core::EntryKind::ToolResult { tool_call_id, .. } if tool_call_id == "c1")
    ));

    // The assembled request is valid: every tool_use has a matching tool_result
    // and nothing gets stripped as orphaned.
    let a = w.assemble();
    let mut uses = std::collections::HashSet::new();
    let mut results = std::collections::HashSet::new();
    for m in &a.messages {
        if let leviath_providers::MessageContent::Blocks(blocks) = &m.content {
            for b in blocks {
                match b {
                    leviath_providers::ContentBlock::ToolUse { id, .. } => {
                        uses.insert(id.clone());
                    }
                    leviath_providers::ContentBlock::ToolResult { tool_use_id, .. } => {
                        results.insert(tool_use_id.clone());
                    }
                    _ => {}
                }
            }
        }
    }
    assert_eq!(
        uses, results,
        "every tool_use must have a matching tool_result"
    );
    assert!(uses.contains("c1"), "the read_file tool_use must survive");
}

#[test]
fn routing_override_matches_bash_alias_to_shell() {
    // Blueprint routes `bash`, but the model calls the canonical `shell`
    // (bash is an alias). The override must still match.
    let mut w = ctx(&[("conversation", 10_000), ("test_results", 10_000)]);
    let r = routing("conversation", &[("bash", "test_results")], true, None);
    apply_tool_results(
        &mut w,
        "run tests",
        &[tc("c1", "shell")],
        &[("c1".to_string(), "All tests passed".to_string())],
        Some(&r),
        None,
    );
    assert!(
        w.get_region("test_results")
            .unwrap()
            .content
            .iter()
            .any(|e| e.content().contains("All tests passed")),
        "a `bash` override must route the canonical `shell` tool's result"
    );
}

#[test]
fn apply_default_region_when_no_override() {
    let mut w = ctx(&[("dflt", 10_000)]);
    let r = routing("dflt", &[], true, None); // no matching override for "read"
    apply_tool_results(
        &mut w,
        "resp",
        &[tc("c1", "read")],
        &[("c1".to_string(), "x".to_string())],
        Some(&r),
        None,
    );
    assert!(w.get_region("dflt").unwrap().current_tokens > 0);
}

#[test]
fn apply_routes_to_scratch_when_not_persist() {
    let mut w = ctx(&[("conversation", 10_000), ("scratch", 10_000)]);
    let r = routing("conversation", &[], false, None); // persist = false
    apply_tool_results(
        &mut w,
        "resp",
        &[tc("c1", "read")],
        &[("c1".to_string(), "x".to_string())],
        Some(&r),
        None,
    );
    assert!(w.get_region("scratch").unwrap().current_tokens > 0);
}

#[test]
fn apply_not_persist_without_scratch_uses_base_region() {
    let mut w = ctx(&[("conversation", 10_000)]); // no scratch region
    let r = routing("conversation", &[], false, None); // persist=false but no scratch
    apply_tool_results(
        &mut w,
        "r",
        &[tc("c1", "read")],
        &[("c1".to_string(), "x".to_string())],
        Some(&r),
        None,
    );
    assert!(w.get_region("conversation").unwrap().current_tokens > 0);
}

#[test]
fn apply_truncates_per_max_result_tokens() {
    let mut w = ctx(&[("conversation", 10_000)]);
    let r = routing("conversation", &[], true, Some(1)); // 1 token ≈ 4 chars
    let long = "x".repeat(100);
    apply_tool_results(
        &mut w,
        "resp",
        &[tc("c1", "read")],
        &[("c1".to_string(), long)],
        Some(&r),
        None,
    );
    // Truncated, so the stored result is far smaller than 100 chars.
    assert!(w.get_region("conversation").unwrap().current_tokens < 25);
}

#[test]
fn apply_no_truncation_when_result_under_max() {
    let mut w = ctx(&[("conversation", 10_000)]);
    let r = routing("conversation", &[], true, Some(100)); // budget 100 tok ≈ 400 chars
    apply_tool_results(
        &mut w,
        "r",
        &[tc("c1", "read")],
        &[("c1".to_string(), "short".to_string())], // 5 chars - under budget
        Some(&r),
        None,
    );
    assert!(w.get_region("conversation").unwrap().current_tokens > 0);
}

#[test]
fn apply_tags_taint_when_sensitivities_present() {
    let mut w = ctx(&[("conversation", 10_000)]);
    let mut sens = std::collections::HashMap::new();
    sens.insert("read".to_string(), leviath_core::TaintLevel::Private);
    apply_tool_results(
        &mut w,
        "resp",
        &[tc("c1", "read")],
        &[("c1".to_string(), "x".to_string())],
        None,
        Some(&sens),
    );
    assert!(w.get_region("conversation").unwrap().current_tokens > 0);
}

#[test]
fn apply_truncates_to_available_when_region_nearly_full() {
    let mut w = ctx(&[("conversation", 200)]);
    // Pre-fill so the tool result can't fit, but >100 tokens remain free.
    w.add_typed_entry(
        "conversation",
        leviath_core::EntryKind::UserMessage,
        "x".repeat(360),
        90,
    )
    .unwrap();
    let big = "y".repeat(600); // ~150 tokens - won't fit the ~110 remaining
    apply_tool_results(
        &mut w,
        "r",
        &[tc("c1", "read")],
        &[("c1".to_string(), big)],
        None,
        None,
    );
    // Result was truncated to fit (not dropped), staying within budget.
    let region = w.get_region("conversation").unwrap();
    assert!(region.current_tokens > 90 && region.current_tokens <= 200);
}

fn run_collect_tools(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(collect_tools);
    s.run(world);
}

#[test]
fn collect_tools_applies_and_loops_back_to_infer() {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolResults(rx));
    let e = world
        .spawn((
            ctx(&[("conversation", 10_000)]),
            crate::components::InferenceResult {
                response: "r".to_string(),
                tool_calls: vec![tc("c1", "read")],
                tokens_used: 0,
                timestamp: 0,
            },
            AwaitingTools,
        ))
        .id();
    tx.send(ToolOutcome {
        elapsed: std::time::Duration::ZERO,
        entity: e,
        results: vec![("c1".to_string(), "res".to_string())],
    })
    .unwrap();

    run_collect_tools(&mut world);

    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<AwaitingTools>(e).is_none());
}

#[test]
fn collect_tools_merges_stashed_context_results() {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolResults(rx));
    let e = world
        .spawn((
            ctx(&[("conversation", 10_000)]),
            infer_with(vec![ctx_call("c1", "notes", "hi"), tc("c2", "read")]),
            ContextToolResults(vec![("c1".to_string(), "stored".to_string())]),
            AwaitingTools,
        ))
        .id();
    tx.send(ToolOutcome {
        elapsed: std::time::Duration::ZERO,
        entity: e,
        results: vec![("c2".to_string(), "file body".to_string())],
    })
    .unwrap();

    run_collect_tools(&mut world);

    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<ContextToolResults>(e).is_none()); // consumed
    // Both results were written into context.
    assert!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .current_tokens
            > 0
    );
}

#[test]
fn collect_tools_drops_stale_outcome() {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolResults(rx));
    let e = world.spawn(ctx(&[("conversation", 10_000)])).id(); // no AwaitingTools
    tx.send(ToolOutcome {
        elapsed: std::time::Duration::ZERO,
        entity: e,
        results: vec![],
    })
    .unwrap();

    run_collect_tools(&mut world);

    assert!(world.get::<ReadyToInfer>(e).is_none());
}

// ── message delivery ──

fn msg(agent_id: &str, content: &str, region: Option<&str>) -> AgentMessage {
    AgentMessage {
        agent_id: agent_id.to_string(),
        content: content.to_string(),
        target_region: region.map(String::from),
        priority: 0,
    }
}

fn run_deliver(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(deliver_messages);
    s.run(world);
}

fn spawn_msg_agent(world: &mut World, accepts: bool, regions: &[(&str, usize)]) -> Entity {
    let mut state = agent_state();
    state.agent_id = "a1".to_string();
    state.accepts_messages = accepts;
    world
        .spawn((state, MessageInbox::default(), ctx(regions)))
        .id()
}

#[test]
fn deliver_routes_and_delivers_to_accepting_agent() {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(MessageIntake(rx));
    let e = spawn_msg_agent(&mut world, true, &[("conversation", 10_000)]);
    tx.send(msg("a1", "hello", None)).unwrap();

    run_deliver(&mut world);

    assert!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .current_tokens
            > 0
    );
    assert!(world.get::<MessageInbox>(e).unwrap().messages.is_empty());
}

#[test]
fn deliver_holds_for_non_accepting_agent() {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(MessageIntake(rx));
    let e = spawn_msg_agent(&mut world, false, &[("conversation", 10_000)]);
    tx.send(msg("a1", "hello", None)).unwrap();

    run_deliver(&mut world);

    // Not delivered - waits in the inbox for a stage that accepts messages.
    assert_eq!(world.get::<MessageInbox>(e).unwrap().messages.len(), 1);
    assert_eq!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .current_tokens,
        0
    );
}

#[test]
fn deliver_drops_message_for_unknown_agent() {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(MessageIntake(rx));
    let e = spawn_msg_agent(&mut world, true, &[("conversation", 10_000)]);
    tx.send(msg("nobody", "hi", None)).unwrap();

    run_deliver(&mut world);

    assert!(world.get::<MessageInbox>(e).unwrap().messages.is_empty());
    assert_eq!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .current_tokens,
        0
    );
}

#[test]
fn deliver_honors_target_region() {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(MessageIntake(rx));
    let e = spawn_msg_agent(
        &mut world,
        true,
        &[("conversation", 10_000), ("notes", 10_000)],
    );
    tx.send(msg("a1", "note this", Some("notes"))).unwrap();

    run_deliver(&mut world);

    assert!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("notes")
            .unwrap()
            .current_tokens
            > 0
    );
}

// ── transition resolution ──

fn edge(
    target: &str,
    cond: leviath_core::blueprint::TransitionCondition,
) -> (String, leviath_core::blueprint::TransitionEdge) {
    (
        target.to_string(),
        leviath_core::blueprint::TransitionEdge {
            target: target.to_string(),
            condition: cond,
            hint: None,
            transform: leviath_core::blueprint::EdgeTransform::Direct,
            gate: None,
            stuck: None,
        },
    )
}

fn stage_named(
    name: &str,
    edges: Option<Vec<(String, leviath_core::blueprint::TransitionEdge)>>,
    allow_complete: bool,
    max_revisits: Option<usize>,
) -> leviath_core::Stage {
    let mut s = leviath_core::Stage::new(
        name.to_string(),
        leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
    );
    s.allow_complete = allow_complete;
    s.max_revisits = max_revisits;
    if let Some(edges) = edges {
        s.transitions = Some(edges.into_iter().collect());
    }
    s
}

fn blueprint(stages: Vec<leviath_core::Stage>) -> leviath_core::Blueprint {
    let layout = leviath_core::layout::ContextLayout::new(
        vec![leviath_core::layout::RegionDefinition::new(
            "conversation".to_string(),
            RegionKind::Clearable,
            10_000,
        )],
        12_000,
    );
    leviath_core::Blueprint::new("t".to_string(), "d".to_string(), stages, layout)
}

fn si(model: &str) -> StageInference {
    StageInference {
        provider_name: "p".to_string(),
        model: model.to_string(),
        tools: vec![],
        tool_filter: None,
    }
}

/// A no-op stage setup (no layout, no system prompt, accepts input).
fn setup() -> StageSetup {
    StageSetup {
        inference_config: InferenceConfig {
            temperature: None,
            max_output_tokens: None,
            extra_params: Default::default(),
            batch_tool_hint: false,
            request_timeout_secs: None,
        },
        routing: None,
        accepts_messages: true,
        context_layout: None,
        system_prompt: None,
    }
}

fn setups(n: usize) -> StageSetups {
    StageSetups((0..n).map(|_| setup()).collect())
}

fn spawn_transition_agent(
    world: &mut World,
    bp: leviath_core::Blueprint,
    stage_infs: Vec<StageInference>,
    visits: VisitCounts,
) -> Entity {
    let n = stage_infs.len();
    world
        .spawn((
            AgentBlueprint(bp),
            StageCursor { index: 0 },
            agent_state(),
            StageProgress {
                total_tool_calls: 3,
                text_only_nudges: 1,
                iterations: 0,
                ..Default::default()
            },
            StageInferences(stage_infs),
            setups(n),
            conv_window(),
            visits,
            ResolveTransition,
        ))
        .id()
}

fn run_transition(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(resolve_transition);
    s.run(world);
}

#[test]
fn transition_linear_advances_to_next_stage() {
    let bp = blueprint(vec![
        stage_named("a", None, false, None),
        stage_named("b", None, false, None),
    ]);
    let mut world = World::new();
    let e = spawn_transition_agent(
        &mut world,
        bp,
        vec![si("m0"), si("m1")],
        VisitCounts::default(),
    );

    run_transition(&mut world);

    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    assert_eq!(world.get::<StageInference>(e).unwrap().model, "m1");
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<ResolveTransition>(e).is_none());
    // Progress reset, visit bumped, current stage updated.
    assert_eq!(world.get::<StageProgress>(e).unwrap().total_tool_calls, 0);
    assert_eq!(world.get::<AgentState>(e).unwrap().current_stage, "b");
    assert_eq!(world.get::<VisitCounts>(e).unwrap().0.get("b"), Some(&1));
}

#[test]
fn transition_terminal_marks_complete() {
    let bp = blueprint(vec![stage_named("only", None, false, None)]);
    let mut world = World::new();
    let e = spawn_transition_agent(&mut world, bp, vec![si("m")], VisitCounts::default());

    run_transition(&mut world);

    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Complete
    );
    assert!(world.get::<ResolveTransition>(e).is_none());
    assert!(world.get::<ReadyToInfer>(e).is_none());
}

#[test]
fn transition_single_graph_edge_advances() {
    use leviath_core::blueprint::TransitionCondition;
    let bp = blueprint(vec![
        stage_named(
            "a",
            Some(vec![edge("b", TransitionCondition::Always)]),
            false,
            None,
        ),
        stage_named("b", None, false, None),
    ]);
    let mut world = World::new();
    let e = spawn_transition_agent(
        &mut world,
        bp,
        vec![si("m0"), si("m1")],
        VisitCounts::default(),
    );

    run_transition(&mut world);

    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    assert!(world.get::<ReadyToInfer>(e).is_some());
}

#[test]
fn transition_empty_transitions_is_terminal() {
    let bp = blueprint(vec![stage_named("a", Some(vec![]), false, None)]);
    let mut world = World::new();
    let e = spawn_transition_agent(&mut world, bp, vec![si("m")], VisitCounts::default());

    run_transition(&mut world);

    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Complete
    );
}

#[test]
fn transition_multiple_edges_awaits_choice() {
    use leviath_core::blueprint::TransitionCondition;
    let bp = blueprint(vec![
        stage_named(
            "a",
            Some(vec![
                edge("b", TransitionCondition::Always),
                edge("c", TransitionCondition::Always),
            ]),
            false,
            None,
        ),
        stage_named("b", None, false, None),
        stage_named("c", None, false, None),
    ]);
    let mut world = World::new();
    let e = spawn_transition_agent(
        &mut world,
        bp,
        vec![si("m0"), si("m1"), si("m2")],
        VisitCounts::default(),
    );

    run_transition(&mut world);

    let choice = world.get::<AwaitingTransitionChoice>(e).unwrap();
    assert_eq!(choice.0.len(), 2);
    assert!(world.get::<ResolveTransition>(e).is_none());
    assert!(world.get::<ReadyToInfer>(e).is_none());
}

#[test]
fn transition_allow_complete_single_edge_awaits_choice() {
    use leviath_core::blueprint::TransitionCondition;
    let bp = blueprint(vec![
        stage_named(
            "a",
            Some(vec![edge("b", TransitionCondition::Always)]),
            true, // allow_complete: LLM must be asked (can say DONE)
            None,
        ),
        stage_named("b", None, false, None),
    ]);
    let mut world = World::new();
    let e = spawn_transition_agent(
        &mut world,
        bp,
        vec![si("m0"), si("m1")],
        VisitCounts::default(),
    );

    run_transition(&mut world);

    assert!(world.get::<AwaitingTransitionChoice>(e).is_some());
}

#[test]
fn transition_visit_exhausted_edge_is_terminal() {
    use leviath_core::blueprint::TransitionCondition;
    let bp = blueprint(vec![
        stage_named(
            "a",
            Some(vec![edge("b", TransitionCondition::Always)]),
            false,
            None,
        ),
        stage_named("b", None, false, Some(0)), // max_revisits 0
    ]);
    let mut visits = VisitCounts::default();
    visits.0.insert("b".to_string(), 1); // already visited past its budget
    let mut world = World::new();
    let e = spawn_transition_agent(&mut world, bp, vec![si("m0"), si("m1")], visits);

    run_transition(&mut world);

    // Only edge exhausted ⇒ terminal.
    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Complete
    );
}

#[test]
fn transition_non_choosable_edge_is_terminal() {
    use leviath_core::blueprint::TransitionCondition;
    let bp = blueprint(vec![
        // Only an Error-condition edge, which isn't followable on a normal
        // completion ⇒ filtered out of the choosable set ⇒ terminal.
        stage_named(
            "a",
            Some(vec![edge("b", TransitionCondition::Error)]),
            false,
            None,
        ),
        stage_named("b", None, false, None),
    ]);
    let mut world = World::new();
    let e = spawn_transition_agent(
        &mut world,
        bp,
        vec![si("m0"), si("m1")],
        VisitCounts::default(),
    );

    run_transition(&mut world);

    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Complete
    );
}

#[test]
fn transition_unknown_target_edge_is_terminal() {
    use leviath_core::blueprint::TransitionCondition;
    let bp = blueprint(vec![stage_named(
        "a",
        Some(vec![edge("ghost", TransitionCondition::Always)]),
        false,
        None,
    )]);
    let mut world = World::new();
    let e = spawn_transition_agent(&mut world, bp, vec![si("m0")], VisitCounts::default());

    run_transition(&mut world);

    // Edge points at a nonexistent stage ⇒ filtered ⇒ terminal.
    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Complete
    );
}

// ── stage setup on entry ──

fn pinned_window() -> ContextWindow {
    let mut w = ContextWindow::new(10_000);
    w.add_region(Region::new("sys".to_string(), RegionKind::Pinned, 2000));
    w.add_region(Region::new(
        "conversation".to_string(),
        RegionKind::Clearable,
        10_000,
    ));
    w
}

/// Spawn a linear two-stage agent poised to transition, with a custom setup
/// for the destination stage and the given starting window.
fn spawn_setup_agent(world: &mut World, dest_setup: StageSetup, window: ContextWindow) -> Entity {
    let bp = blueprint(vec![
        stage_named("a", None, false, None),
        stage_named("b", None, false, None),
    ]);
    world
        .spawn((
            AgentBlueprint(bp),
            StageCursor { index: 0 },
            agent_state(),
            StageProgress::default(),
            StageInferences(vec![si("m0"), si("m1")]),
            StageSetups(vec![setup(), dest_setup]),
            VisitCounts::default(),
            window,
            ResolveTransition,
        ))
        .id()
}

#[test]
fn enter_stage_injects_system_prompt_and_config() {
    let mut s = setup();
    s.system_prompt = Some("be terse".to_string());
    s.inference_config = InferenceConfig {
        temperature: Some(0.3),
        max_output_tokens: Some(99),
        extra_params: Default::default(),
        batch_tool_hint: false,
        request_timeout_secs: None,
    };
    s.accepts_messages = false;
    let mut world = World::new();
    let e = spawn_setup_agent(&mut world, s, pinned_window());

    run_transition(&mut world);

    // Instructions landed in the pinned region, not conversation.
    assert!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("sys")
            .unwrap()
            .current_tokens
            > 0
    );
    let cfg = world.get::<InferenceConfig>(e).unwrap();
    assert_eq!(cfg.max_output_tokens, Some(99));
    assert!(!world.get::<AgentState>(e).unwrap().accepts_messages);
    assert!(world.get::<ReadyToInfer>(e).is_some());
}

#[test]
fn enter_stage_swaps_context_layout() {
    let mut s = setup();
    s.context_layout = Some(leviath_core::layout::ContextLayout::new(
        vec![leviath_core::layout::RegionDefinition::new(
            "scratch".to_string(),
            RegionKind::Clearable,
            5000,
        )],
        8000,
    ));
    let mut world = World::new();
    let e = spawn_setup_agent(&mut world, s, pinned_window());

    run_transition(&mut world);

    let w = world.get::<ContextWindow>(e).unwrap();
    assert!(w.get_region("scratch").is_some()); // swapped in
    assert!(w.get_region("sys").is_none()); // old layout dropped
}

#[test]
fn enter_stage_inserts_tool_result_routing() {
    let mut s = setup();
    s.routing = Some(leviath_core::ToolResultRouting {
        default_region: "notes".to_string(),
        ..Default::default()
    });
    let mut world = World::new();
    let e = spawn_setup_agent(&mut world, s, pinned_window());

    run_transition(&mut world);

    let routing = world
        .get::<crate::components::ToolResultRoutingComponent>(e)
        .unwrap();
    assert_eq!(routing.routing.default_region, "notes");
}

#[test]
fn enter_stage_errors_when_system_prompt_overflows_region() {
    let mut s = setup();
    s.system_prompt = Some("x".repeat(100_000)); // far exceeds the 2000-tok region
    let mut world = World::new();
    let e = spawn_setup_agent(&mut world, s, pinned_window());

    run_transition(&mut world);

    assert_eq!(
        std::mem::discriminant(&world.get::<AgentState>(e).unwrap().status),
        std::mem::discriminant(&AgentStatus::Error {
            message: String::new()
        })
    );
    assert!(world.get::<ReadyToInfer>(e).is_none());
}

#[test]
fn enter_stage_without_target_region_skips_injection() {
    // Neither a pinned region nor a "conversation" region exists, so the
    // stage-instructions target ("conversation" fallback) isn't found: the
    // clear is skipped and, with no system prompt, entry still succeeds.
    let mut w = ContextWindow::new(10_000);
    w.add_region(Region::new(
        "notes".to_string(),
        RegionKind::Clearable,
        5000,
    ));
    let mut world = World::new();
    let e = spawn_setup_agent(&mut world, setup(), w);

    run_transition(&mut world);

    assert!(world.get::<ReadyToInfer>(e).is_some());
}

#[test]
fn collect_choice_errors_when_system_prompt_overflows() {
    let (mut world, tx) = world_with_transition_results();
    let bp = blueprint(vec![
        stage_named("a", None, false, None),
        stage_named("b", None, false, None),
    ]);
    let mut dest = setup();
    dest.system_prompt = Some("x".repeat(100_000));
    let e = world
        .spawn((
            AgentBlueprint(bp),
            StageCursor { index: 0 },
            agent_state(),
            StageProgress::default(),
            StageInferences(vec![si("m0"), si("m1")]),
            StageSetups(vec![setup(), dest]),
            VisitCounts::default(),
            pinned_window(),
            AwaitingTransitionResponse(vec![plain_edge("b")]),
        ))
        .id();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Ok(resp("b")),
    })
    .unwrap();

    run_collect_transition(&mut world);

    assert_eq!(
        std::mem::discriminant(&world.get::<AgentState>(e).unwrap().status),
        std::mem::discriminant(&AgentStatus::Error {
            message: String::new()
        })
    );
    assert!(world.get::<AwaitingTransitionResponse>(e).is_none());
}

// ── agent spawn (blueprint → components) ──

fn resolved(model: &str) -> ResolvedStage {
    ResolvedStage {
        provider_name: "p".to_string(),
        model: model.to_string(),
        tools: vec![],
    }
}

#[test]
fn spawn_agent_builds_stage0_ready_with_config_and_routing() {
    // A stage with model parameters, routing, and a system prompt should
    // produce a ready agent carrying all of them.
    let layout = leviath_core::layout::ContextLayout::new(
        vec![leviath_core::layout::RegionDefinition::new(
            "task".to_string(),
            RegionKind::Pinned,
            4000,
        )],
        8000,
    );
    let mut s = leviath_core::Stage::new(
        "start".to_string(),
        leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
    );
    s.model
        .parameters
        .insert("temperature".to_string(), serde_json::json!(0.5));
    s.model
        .parameters
        .insert("max_output_tokens".to_string(), serde_json::json!(128));
    s.config.insert(
        "system_prompt".to_string(),
        serde_json::Value::String("be helpful".to_string()),
    );
    s.tool_result_routing = Some(leviath_core::ToolResultRouting {
        default_region: "notes".to_string(),
        ..Default::default()
    });
    let bp = leviath_core::Blueprint::new("t".to_string(), "d".to_string(), vec![s], layout);

    let mut world = World::new();
    let e = spawn_agent(
        &mut world,
        "agent-x".to_string(),
        bp,
        "the task",
        vec![resolved("m")],
        true,
    )
    .unwrap();

    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 0);
    let cfg = world.get::<InferenceConfig>(e).unwrap();
    assert_eq!(cfg.temperature, Some(0.5));
    assert_eq!(cfg.max_output_tokens, Some(128));
    assert_eq!(
        world
            .get::<crate::components::ToolResultRoutingComponent>(e)
            .unwrap()
            .routing
            .default_region,
        "notes"
    );
    assert_eq!(world.get::<AgentState>(e).unwrap().agent_id, "agent-x");
    // Stage 0's visit is pre-counted.
    assert_eq!(
        world.get::<VisitCounts>(e).unwrap().0.get("start"),
        Some(&1)
    );
    // Task text + system prompt both seeded the pinned region.
    assert!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("task")
            .unwrap()
            .current_tokens
            > 0
    );
}

#[test]
fn spawn_agent_defaults_config_and_no_routing() {
    // No parameters, no routing, no system prompt → default config, no
    // routing component.
    let bp = blueprint(vec![stage_named("only", None, false, None)]);
    let mut world = World::new();
    let e = spawn_agent(
        &mut world,
        "a".to_string(),
        bp,
        "t",
        vec![resolved("m")],
        true,
    )
    .unwrap();

    let cfg = world.get::<InferenceConfig>(e).unwrap();
    assert_eq!(cfg.temperature, None);
    assert_eq!(cfg.max_output_tokens, None);
    assert!(
        world
            .get::<crate::components::ToolResultRoutingComponent>(e)
            .is_none()
    );
}

#[test]
fn stage_setup_from_folds_fanout_split_prompt() {
    use leviath_core::blueprint::{FanOutConfig, StageMode, WorkerFailurePolicy};
    let fanout = |split: &str| StageMode::FanOut {
        config: FanOutConfig {
            worker_agent: None,
            worker_stage: Some("w".to_string()),
            worker_query: None,
            merge_stage: None,
            max_workers: 4,
            on_worker_failure: WorkerFailurePolicy::Continue,
            split_prompt: split.to_string(),
        },
    };

    // Fan-out stage with a base prompt: split prompt is appended.
    let mut s = stage_named("fan", None, false, None);
    s.mode = fanout("SPLIT NOW");
    s.config.insert(
        "system_prompt".to_string(),
        serde_json::Value::String("base instructions".to_string()),
    );
    let sp = stage_setup_from(&s, true, None).system_prompt.unwrap();
    assert!(sp.contains("base instructions") && sp.contains("SPLIT NOW"));

    // Fan-out stage with no base prompt: the split prompt alone.
    let mut s2 = stage_named("fan", None, false, None);
    s2.mode = fanout("ONLY SPLIT");
    assert_eq!(
        stage_setup_from(&s2, true, None).system_prompt,
        Some("ONLY SPLIT".to_string())
    );

    // Fan-out stage with an empty split prompt: base prompt is left as-is.
    let mut s3 = stage_named("fan", None, false, None);
    s3.mode = fanout("   ");
    assert_eq!(stage_setup_from(&s3, true, None).system_prompt, None);
}

#[test]
fn stage_setup_from_collects_extra_model_parameters() {
    let mut s = stage_named("plan", None, false, None);
    // temperature/max_output_tokens are consumed specially; everything else
    // is collected as pass-through extra_params.
    s.model
        .parameters
        .insert("temperature".to_string(), serde_json::json!(0.3));
    s.model
        .parameters
        .insert("max_output_tokens".to_string(), serde_json::json!(256));
    s.model
        .parameters
        .insert("top_p".to_string(), serde_json::json!(0.9));
    s.model
        .parameters
        .insert("seed".to_string(), serde_json::json!(11));

    let setup = stage_setup_from(&s, true, None);
    assert_eq!(setup.inference_config.temperature, Some(0.3));
    assert_eq!(setup.inference_config.max_output_tokens, Some(256));
    let extra = &setup.inference_config.extra_params;
    assert_eq!(extra.len(), 2);
    assert_eq!(extra["top_p"], serde_json::json!(0.9));
    assert_eq!(extra["seed"], serde_json::json!(11));
    assert!(!extra.contains_key("temperature"));
}

#[test]
fn stage_setup_from_threads_request_timeout() {
    // Unset on the stage → None on the inference config.
    let s = stage_named("plan", None, false, None);
    assert_eq!(
        stage_setup_from(&s, true, None)
            .inference_config
            .request_timeout_secs,
        None
    );

    // Set on the stage's model → carried onto the inference config verbatim.
    let mut s2 = stage_named("plan", None, false, None);
    s2.model.request_timeout_secs = Some(300);
    assert_eq!(
        stage_setup_from(&s2, true, None)
            .inference_config
            .request_timeout_secs,
        Some(300)
    );
}

#[test]
fn retry_policy_for_overrides_job_timeout_when_set() {
    let default = crate::inference_bridge::RetryPolicy::default();

    // No config at all → default policy unchanged.
    assert_eq!(retry_policy_for(None).job_timeout, default.job_timeout);

    // Config present but no per-stage timeout → default still stands.
    let cfg_none = InferenceConfig {
        request_timeout_secs: None,
        ..Default::default()
    };
    assert_eq!(
        retry_policy_for(Some(&cfg_none)).job_timeout,
        default.job_timeout
    );

    // Per-stage timeout set → job_timeout is overridden to that value, other
    // retry fields left at their defaults.
    let cfg_some = InferenceConfig {
        request_timeout_secs: Some(120),
        ..Default::default()
    };
    let policy = retry_policy_for(Some(&cfg_some));
    assert_eq!(policy.job_timeout, std::time::Duration::from_secs(120));
    assert_eq!(policy.max_attempts, default.max_attempts);
    assert_eq!(policy.base_delay, default.base_delay);
}

#[test]
fn spawn_agent_errors_on_oversized_system_prompt() {
    let layout = leviath_core::layout::ContextLayout::new(
        vec![leviath_core::layout::RegionDefinition::new(
            "task".to_string(),
            RegionKind::Pinned,
            40,
        )],
        1000,
    );
    let mut s = leviath_core::Stage::new(
        "only".to_string(),
        leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
    );
    s.config.insert(
        "system_prompt".to_string(),
        serde_json::Value::String("z".repeat(100_000)),
    );
    let bp = leviath_core::Blueprint::new("t".to_string(), "d".to_string(), vec![s], layout);

    let mut world = World::new();
    let err = spawn_agent(
        &mut world,
        "a".to_string(),
        bp,
        "t",
        vec![resolved("m")],
        true,
    );
    assert!(err.is_err());
}

// ── compaction ──

fn compacting_window() -> ContextWindow {
    let mut w = ContextWindow::new(100);
    let mut conv = Region::new(
        "conv".to_string(),
        RegionKind::Compacting {
            threshold_tokens: 5,
        },
        100,
    );
    let _ = conv.add_entry("x".repeat(380), 95); // 95 tokens: over threshold, <10 free
    w.add_region(conv);
    w.add_region(Region::new(
        "history".to_string(),
        RegionKind::CompactHistory {
            source_region: "conv".to_string(),
        },
        100,
    ));
    w.current_tokens = w.calculate_tokens();
    w
}

fn compaction_settings(provider: &str, model: &str) -> CompactionSettings {
    CompactionSettings(leviath_core::CompactionConfig {
        provider: provider.to_string(),
        model: model.to_string(),
        system_prompt: None,
        user_prompt_template: None,
        max_summary_tokens: 200,
        temperature: 0.2,
    })
}

fn run_dispatch_compaction(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(dispatch_compaction);
    s.run(world);
}

#[tokio::test]
async fn compaction_dispatches_when_over_threshold() {
    // Provider "cfg" is registered by build_world; the window is at the
    // eviction threshold with a Compacting region that needs summarizing.
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let e = world
        .spawn((
            compacting_window(),
            compaction_settings("cfg", "m"),
            agent_state(),
            ReadyToInfer,
        ))
        .id();

    run_dispatch_compaction(&mut world);

    assert!(world.get::<AwaitingCompaction>(e).is_some());
    assert!(world.get::<ReadyToInfer>(e).is_none());
}

#[tokio::test]
async fn compaction_skips_non_active_agent() {
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let mut st = agent_state();
    st.status = AgentStatus::Idle;
    let e = world
        .spawn((
            compacting_window(),
            compaction_settings("cfg", "m"),
            st,
            ReadyToInfer,
        ))
        .id();

    run_dispatch_compaction(&mut world);

    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<AwaitingCompaction>(e).is_none());
}

#[tokio::test]
async fn compaction_skips_when_under_threshold() {
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let mut w = ContextWindow::new(1000);
    w.add_region(Region::new(
        "conv".to_string(),
        RegionKind::Compacting {
            threshold_tokens: 5,
        },
        1000,
    ));
    let e = world
        .spawn((
            w,
            compaction_settings("cfg", "m"),
            agent_state(),
            ReadyToInfer,
        ))
        .id();

    run_dispatch_compaction(&mut world);

    // Under threshold ⇒ untouched, ready to infer.
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<AwaitingCompaction>(e).is_none());
}

#[tokio::test]
async fn compaction_skips_when_provider_missing() {
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let e = world
        .spawn((
            compacting_window(),
            compaction_settings("ghost", "m"), // unregistered provider
            agent_state(),
            ReadyToInfer,
        ))
        .id();

    run_dispatch_compaction(&mut world);

    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<AwaitingCompaction>(e).is_none());
}

#[tokio::test]
async fn compaction_skips_when_pool_full() {
    let mut cfg = InferencePoolConfig::new();
    cfg.set_limit("m", 0); // no permits for the compaction model
    let (mut world, _rx) = build_world(InferencePools::new(cfg));
    let e = world
        .spawn((
            compacting_window(),
            compaction_settings("cfg", "m"),
            agent_state(),
            ReadyToInfer,
        ))
        .id();

    run_dispatch_compaction(&mut world);

    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<AwaitingCompaction>(e).is_none());
}

#[tokio::test]
async fn compaction_evicts_but_needs_no_summary() {
    // A Clearable region over threshold is fully cleared by sync eviction, so
    // no LLM summary is needed and the agent stays ready to infer.
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let mut w = ContextWindow::new(100);
    let mut scratch = Region::new("scratch".to_string(), RegionKind::Clearable, 100);
    let _ = scratch.add_entry("y".repeat(360), 95);
    w.add_region(scratch);
    w.current_tokens = w.calculate_tokens();
    let e = world
        .spawn((
            w,
            compaction_settings("cfg", "m"),
            agent_state(),
            ReadyToInfer,
        ))
        .id();

    run_dispatch_compaction(&mut world);

    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<AwaitingCompaction>(e).is_none());
    // The clearable region was emptied by eviction.
    assert_eq!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("scratch")
            .unwrap()
            .current_tokens,
        0
    );
}

#[tokio::test]
async fn compaction_skips_when_eviction_errors() {
    // Pinned content over the total budget makes try_evict return
    // PinnedRegionsOverBudget; compaction is skipped and inference proceeds.
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let mut w = ContextWindow::new(100);
    let mut pinned = Region::new("id".to_string(), RegionKind::Pinned, 500);
    let _ = pinned.add_entry("p".repeat(600), 150); // pinned 150 > budget 100
    w.add_region(pinned);
    w.current_tokens = w.calculate_tokens();
    let e = world
        .spawn((
            w,
            compaction_settings("cfg", "m"),
            agent_state(),
            ReadyToInfer,
        ))
        .id();

    run_dispatch_compaction(&mut world);

    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<AwaitingCompaction>(e).is_none());
}

#[tokio::test]
async fn compaction_skips_region_with_empty_content() {
    // A Compacting region over its token threshold but whose entries carry no
    // text (a token-only placeholder) yields nothing to summarize.
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let mut w = ContextWindow::new(100);
    let mut conv = Region::new(
        "conv".to_string(),
        RegionKind::Compacting {
            threshold_tokens: 5,
        },
        100,
    );
    let _ = conv.add_entry(String::new(), 95); // empty content, 95 tokens
    w.add_region(conv);
    w.current_tokens = w.calculate_tokens();
    let e = world
        .spawn((
            w,
            compaction_settings("cfg", "m"),
            agent_state(),
            ReadyToInfer,
        ))
        .id();

    run_dispatch_compaction(&mut world);

    // Nothing summarizable ⇒ no job, stays ready.
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<AwaitingCompaction>(e).is_none());
}

// ── edge transforms ──

use leviath_core::blueprint::EdgeTransform;

/// A window with a pinned `sys` region and a stage-specific `scratch` region,
/// both with content.
fn transform_window() -> ContextWindow {
    let mut w = ContextWindow::new(1000);
    let mut sys = Region::new("sys".to_string(), RegionKind::Pinned, 500);
    let _ = sys.add_entry("identity", 10);
    w.add_region(sys);
    let mut scratch = Region::new("scratch".to_string(), RegionKind::Clearable, 500);
    let _ = scratch.add_entry("work", 10);
    w.add_region(scratch);
    w.current_tokens = w.calculate_tokens();
    w
}

#[test]
fn apply_edge_transform_direct_is_a_noop() {
    let mut w = transform_window();
    let before = w.current_tokens;
    assert!(apply_edge_transform(&mut w, &EdgeTransform::Direct).is_empty());
    assert_eq!(w.current_tokens, before);
    assert!(w.get_region("scratch").unwrap().current_tokens > 0);
}

#[test]
fn apply_edge_transform_clear_wipes_stage_specific_keeps_pinned() {
    let mut w = transform_window();
    assert!(apply_edge_transform(&mut w, &EdgeTransform::Clear).is_empty());
    assert_eq!(w.get_region("scratch").unwrap().current_tokens, 0);
    assert!(w.get_region("sys").unwrap().current_tokens > 0);
}

#[test]
fn edge_transforms_respect_custom_region_persistence() {
    // Non-persistent custom is stage-specific (wiped by Clear); persistent is
    // protected alongside Pinned/HashMap/CompactHistory.
    let mut w = transform_window();
    let mut scratch_custom = Region::new(
        "scratch_custom".to_string(),
        RegionKind::Custom {
            script: "s.rhai".to_string(),
            persistent: false,
        },
        500,
    );
    let _ = scratch_custom.add_entry("wipe me", 10);
    w.add_region(scratch_custom);
    let mut vault = Region::new(
        "vault".to_string(),
        RegionKind::Custom {
            script: "v.rhai".to_string(),
            persistent: true,
        },
        500,
    );
    let _ = vault.add_entry("keep me", 10);
    w.add_region(vault);
    w.current_tokens = w.calculate_tokens();

    assert!(apply_edge_transform(&mut w, &EdgeTransform::Clear).is_empty());
    assert_eq!(w.get_region("scratch_custom").unwrap().current_tokens, 0);
    assert!(w.get_region("vault").unwrap().current_tokens > 0);
}

#[test]
fn apply_edge_transform_compact_returns_stage_specific_with_content() {
    let mut w = transform_window();
    // Pinned excluded; scratch (stage-specific, has content) returned; not cleared.
    assert_eq!(
        apply_edge_transform(&mut w, &EdgeTransform::Compact { prompt: None }),
        vec!["scratch".to_string()]
    );
    assert!(w.get_region("scratch").unwrap().current_tokens > 0);
}

#[test]
fn apply_edge_transform_custom_respects_carry_clear_and_compact() {
    let mut w = transform_window();
    let mut keep = Region::new("keep".to_string(), RegionKind::Clearable, 500);
    let _ = keep.add_entry("keepme", 10);
    w.add_region(keep);
    let mut drop = Region::new("drop".to_string(), RegionKind::Clearable, 500);
    let _ = drop.add_entry("dropme", 10);
    w.add_region(drop);
    w.current_tokens = w.calculate_tokens();

    let transform = EdgeTransform::Custom {
        carry: vec!["keep".to_string()],
        // scratch has content ⇒ kept; keep excluded (carry); ghost absent ⇒ filtered.
        compact: vec![
            "scratch".to_string(),
            "keep".to_string(),
            "ghost".to_string(),
        ],
        // drop cleared; keep protected by carry; missing region is a no-op.
        clear: vec![
            "drop".to_string(),
            "keep".to_string(),
            "missing".to_string(),
        ],
        compact_prompt: None,
    };
    let out = apply_edge_transform(&mut w, &transform);
    assert_eq!(w.get_region("drop").unwrap().current_tokens, 0);
    assert!(w.get_region("keep").unwrap().current_tokens > 0);
    assert_eq!(out, vec!["scratch".to_string()]);
}

/// A window with a stage-specific `scratch` region carrying summarizable text.
fn scratch_window() -> ContextWindow {
    let mut w = ContextWindow::new(1000);
    let mut scratch = Region::new("scratch".to_string(), RegionKind::Clearable, 500);
    let _ = scratch.add_entry("work to summarize", 20);
    w.add_region(scratch);
    w.current_tokens = w.calculate_tokens();
    w
}

fn run_dispatch_edge_compact(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(dispatch_edge_compact);
    s.run(world);
}

#[tokio::test]
async fn edge_compact_dispatches_to_the_compaction_lane() {
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let e = world
        .spawn((
            scratch_window(),
            PendingEdgeCompact(vec!["scratch".to_string()]),
            compaction_settings("cfg", "m"),
            agent_state(),
            ReadyToInfer,
        ))
        .id();
    run_dispatch_edge_compact(&mut world);
    assert!(world.get::<AwaitingCompaction>(e).is_some());
    assert!(world.get::<ReadyToInfer>(e).is_none());
    assert!(world.get::<PendingEdgeCompact>(e).is_none());
}

#[tokio::test]
async fn edge_compact_skips_non_active_agent() {
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let mut st = agent_state();
    st.status = AgentStatus::Cancelled;
    let e = world
        .spawn((
            scratch_window(),
            PendingEdgeCompact(vec!["scratch".to_string()]),
            compaction_settings("cfg", "m"),
            st,
            ReadyToInfer,
        ))
        .id();
    run_dispatch_edge_compact(&mut world);
    // Left untouched (marker preserved) for when it resumes.
    assert!(world.get::<PendingEdgeCompact>(e).is_some());
    assert!(world.get::<AwaitingCompaction>(e).is_none());
}

#[tokio::test]
async fn edge_compact_drops_marker_without_compaction_settings() {
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let e = world
        .spawn((
            scratch_window(),
            PendingEdgeCompact(vec!["scratch".to_string()]),
            agent_state(),
            ReadyToInfer,
        ))
        .id();
    run_dispatch_edge_compact(&mut world);
    // No settings ⇒ can't summarize ⇒ drop the request, proceed to inference.
    assert!(world.get::<PendingEdgeCompact>(e).is_none());
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<AwaitingCompaction>(e).is_none());
}

#[tokio::test]
async fn edge_compact_drops_marker_when_nothing_to_summarize() {
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    // A present-but-empty region + an absent region ⇒ no requests.
    let mut w = ContextWindow::new(1000);
    let mut empty = Region::new("empty".to_string(), RegionKind::Clearable, 500);
    let _ = empty.add_entry(String::new(), 5);
    w.add_region(empty);
    let e = world
        .spawn((
            w,
            PendingEdgeCompact(vec!["empty".to_string(), "ghost".to_string()]),
            compaction_settings("cfg", "m"),
            agent_state(),
            ReadyToInfer,
        ))
        .id();
    run_dispatch_edge_compact(&mut world);
    assert!(world.get::<PendingEdgeCompact>(e).is_none());
    assert!(world.get::<AwaitingCompaction>(e).is_none());
}

#[tokio::test]
async fn edge_compact_drops_marker_when_provider_missing() {
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let e = world
        .spawn((
            scratch_window(),
            PendingEdgeCompact(vec!["scratch".to_string()]),
            compaction_settings("ghost", "m"), // unregistered provider
            agent_state(),
            ReadyToInfer,
        ))
        .id();
    run_dispatch_edge_compact(&mut world);
    assert!(world.get::<PendingEdgeCompact>(e).is_none());
    assert!(world.get::<AwaitingCompaction>(e).is_none());
}

#[tokio::test]
async fn edge_compact_drops_marker_when_pool_full() {
    let mut cfg = InferencePoolConfig::new();
    cfg.set_limit("m", 0);
    let (mut world, _rx) = build_world(InferencePools::new(cfg));
    let e = world
        .spawn((
            scratch_window(),
            PendingEdgeCompact(vec!["scratch".to_string()]),
            compaction_settings("cfg", "m"),
            agent_state(),
            ReadyToInfer,
        ))
        .id();
    run_dispatch_edge_compact(&mut world);
    assert!(world.get::<PendingEdgeCompact>(e).is_none());
    assert!(world.get::<AwaitingCompaction>(e).is_none());
}

fn clear_edge(target: &str) -> leviath_core::blueprint::TransitionEdge {
    leviath_core::blueprint::TransitionEdge {
        target: target.to_string(),
        condition: leviath_core::blueprint::TransitionCondition::Always,
        hint: None,
        transform: EdgeTransform::Clear,
        gate: None,
        stuck: None,
    }
}

#[test]
fn resolve_transition_applies_the_edge_clear_transform() {
    let a = stage_named(
        "a",
        Some(vec![("go".to_string(), clear_edge("b"))]),
        false,
        None,
    );
    let b = stage_named("b", None, false, None);
    let bp = blueprint(vec![a, b]);
    let mut world = World::new();
    let e = spawn_transition_agent(
        &mut world,
        bp,
        vec![stage("m", vec![], None), stage("m", vec![], None)],
        VisitCounts::default(),
    );
    // Seed content so the Clear transform has something to wipe.
    world
        .get_mut::<ContextWindow>(e)
        .unwrap()
        .add_to_region("conversation", "chatter".to_string(), 10)
        .unwrap();
    run_transition(&mut world);

    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1); // entered b
    assert_eq!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .current_tokens,
        0 // Clear transform wiped it
    );
    assert!(world.get::<PendingEdgeCompact>(e).is_none()); // Clear needs no LLM
}

#[test]
fn resolve_transition_with_compact_transform_marks_pending_edge_compact() {
    let mut edge = clear_edge("b");
    edge.transform = EdgeTransform::Compact { prompt: None };
    let a = stage_named("a", Some(vec![("go".to_string(), edge)]), false, None);
    let b = stage_named("b", None, false, None);
    let bp = blueprint(vec![a, b]);
    let mut world = World::new();
    let e = spawn_transition_agent(
        &mut world,
        bp,
        vec![stage("m", vec![], None), stage("m", vec![], None)],
        VisitCounts::default(),
    );
    world
        .get_mut::<ContextWindow>(e)
        .unwrap()
        .add_to_region("conversation", "summarize me".to_string(), 10)
        .unwrap();
    run_transition(&mut world);

    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    // The Compact transform queued the conversation region for the LLM lane.
    let pending = world.get::<PendingEdgeCompact>(e).unwrap();
    assert_eq!(pending.0, vec!["conversation".to_string()]);
}

// ── max_iterations + error/max-iter edges (#3+#4) ──

use leviath_core::blueprint::TransitionCondition;

fn conditioned_edge(
    target: &str,
    condition: TransitionCondition,
) -> leviath_core::blueprint::TransitionEdge {
    let mut e = plain_edge(target);
    e.condition = condition;
    e
}

fn spawn_ready_agent(
    world: &mut World,
    max_iterations: Option<usize>,
    iterations: usize,
    status: AgentStatus,
) -> Entity {
    let mut s = stage_named("a", None, false, None);
    s.max_iterations = max_iterations;
    let bp = blueprint(vec![s]);
    world
        .spawn((
            AgentBlueprint(bp),
            StageCursor { index: 0 },
            AgentState {
                status,
                ..agent_state()
            },
            StageProgress {
                iterations,
                ..Default::default()
            },
            ReadyToInfer,
        ))
        .id()
}

fn run_enforce(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(enforce_max_iterations);
    s.run(world);
}

#[test]
fn enforce_max_iterations_caps_at_the_limit() {
    let mut world = World::new();
    let e = spawn_ready_agent(&mut world, Some(3), 3, AgentStatus::Active);
    world
        .entity_mut(e)
        .insert(crate::persistence::RunOutcomeFlags::default());
    // An agent with no flags component still gets capped; there's just
    // nowhere to record it.
    let unflagged = spawn_ready_agent(&mut world, Some(3), 3, AgentStatus::Active);
    run_enforce(&mut world);
    assert!(world.get::<ResolveTransition>(unflagged).is_some());
    assert!(world.get::<ResolveTransition>(e).is_some());
    assert!(world.get::<ReadyToInfer>(e).is_none());
    assert_eq!(
        world.get::<StageOutcome>(e).unwrap(),
        &StageOutcome::MaxIterations
    );
    // The run records it: a stage that ran out of iterations is one way a
    // run ends up with nothing to show (issue #107).
    assert_eq!(
        world
            .get::<crate::persistence::RunOutcomeFlags>(e)
            .unwrap()
            .0
            .max_iterations_hit,
        1
    );
}

#[test]
fn enforce_max_iterations_below_limit_or_unlimited_or_paused_is_noop() {
    let mut world = World::new();
    let below = spawn_ready_agent(&mut world, Some(5), 2, AgentStatus::Active);
    let unlimited = spawn_ready_agent(&mut world, None, 99, AgentStatus::Active);
    let zero = spawn_ready_agent(&mut world, Some(0), 99, AgentStatus::Active);
    let paused = spawn_ready_agent(&mut world, Some(1), 99, AgentStatus::Idle);
    run_enforce(&mut world);
    for e in [below, unlimited, zero, paused] {
        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<ResolveTransition>(e).is_none());
    }
}

// ── stuck detection (#106) ──────────────────────────────────────────────

fn stuck_cfg(
    iterations: Option<usize>,
    minutes: Option<usize>,
    edits: Option<usize>,
    tool_calls: Option<usize>,
) -> leviath_core::blueprint::StuckConfig {
    leviath_core::blueprint::StuckConfig {
        after_iterations: iterations,
        after_minutes: minutes,
        after_same_file_edits: edits,
        after_tool_calls: tool_calls,
    }
}

fn edits(pairs: &[(&str, usize)]) -> std::collections::HashMap<String, usize> {
    pairs.iter().map(|(p, n)| ((*p).to_string(), *n)).collect()
}

#[test]
fn detect_stuck_returns_none_when_no_threshold_trips() {
    // Every threshold set, every metric below it.
    let cfg = stuck_cfg(Some(20), Some(10), Some(5), Some(60));
    let m = StuckMetrics {
        iterations: 19,
        elapsed_secs: 9 * 60,
        tool_calls: 59,
        hottest_edit: Some(("a.rs".to_string(), 4)),
    };
    assert!(detect_stuck(&cfg, &m).is_none());
    // An unarmed config never trips, however bad the metrics look.
    let wild = StuckMetrics {
        iterations: 999,
        elapsed_secs: 999_999,
        tool_calls: 999,
        hottest_edit: Some(("a.rs".to_string(), 999)),
    };
    assert!(detect_stuck(&Default::default(), &wild).is_none());
}

/// File churn wins over the other triggers because it names the actual
/// mistake ("you are editing the wrong file") rather than a symptom.
#[test]
fn detect_stuck_reports_same_file_churn_first() {
    let cfg = stuck_cfg(Some(1), Some(0), Some(3), Some(1));
    let m = StuckMetrics {
        iterations: 50,
        elapsed_secs: 3600,
        tool_calls: 50,
        hottest_edit: Some(("where.py".to_string(), 4)),
    };
    let reason = detect_stuck(&cfg, &m).expect("churn trips");
    assert!(reason.contains("where.py"), "got: {reason}");
    assert!(reason.contains('4'), "got: {reason}");
}

/// The churn threshold must not fire when no file was edited at all -
/// `hottest_edit` is `None` and the next trigger takes over.
#[test]
fn detect_stuck_falls_through_churn_when_nothing_was_edited() {
    let cfg = stuck_cfg(Some(20), None, Some(3), None);
    let m = StuckMetrics {
        iterations: 20,
        hottest_edit: None,
        ..Default::default()
    };
    let reason = detect_stuck(&cfg, &m).expect("iterations trip");
    assert!(reason.contains("20 inference turns"), "got: {reason}");
}

#[test]
fn detect_stuck_reports_iterations_tool_calls_and_minutes() {
    let iters = detect_stuck(
        &stuck_cfg(Some(20), None, None, None),
        &StuckMetrics {
            iterations: 20,
            ..Default::default()
        },
    )
    .expect("iterations trip");
    assert!(iters.contains("20 inference turns"), "got: {iters}");

    let calls = detect_stuck(
        &stuck_cfg(None, None, None, Some(60)),
        &StuckMetrics {
            tool_calls: 61,
            ..Default::default()
        },
    )
    .expect("tool calls trip");
    assert!(calls.contains("61 tool calls"), "got: {calls}");

    let mins = detect_stuck(
        &stuck_cfg(None, Some(10), None, None),
        &StuckMetrics {
            elapsed_secs: 11 * 60,
            ..Default::default()
        },
    )
    .expect("minutes trip");
    assert!(mins.contains("11 minutes"), "got: {mins}");
}

#[test]
fn hottest_edit_is_none_when_empty_and_deterministic_on_ties() {
    assert!(hottest_edit(&std::collections::HashMap::new()).is_none());
    assert_eq!(
        hottest_edit(&edits(&[("a.rs", 1), ("b.rs", 3)])),
        Some(("b.rs".to_string(), 3))
    );
    // Equal counts must resolve the same way every run, whatever order the
    // HashMap iterates in.
    let tie = edits(&[("a.rs", 2), ("b.rs", 2), ("c.rs", 2)]);
    for _ in 0..8 {
        assert_eq!(hottest_edit(&tie), Some(("a.rs".to_string(), 2)));
    }
}

#[test]
fn edited_path_matches_only_mutating_tools_with_a_string_path() {
    let call = |name: &str, args: serde_json::Value| crate::components::ToolCall {
        tool_id: "1".to_string(),
        name: name.to_string(),
        arguments: args,
        thought_signature: None,
    };
    let with_path = serde_json::json!({ "path": "src/main.rs" });
    assert_eq!(
        edited_path(&call("write_file", with_path.clone())),
        Some("src/main.rs")
    );
    assert_eq!(
        edited_path(&call("edit_file", with_path.clone())),
        Some("src/main.rs")
    );
    // Reads don't count as churn, and a mutating call without a usable
    // path contributes nothing rather than panicking.
    assert!(edited_path(&call("read_file", with_path)).is_none());
    assert!(edited_path(&call("write_file", serde_json::json!({}))).is_none());
    assert!(edited_path(&call("write_file", serde_json::json!({ "path": 7 }))).is_none());
}

#[test]
fn note_stuck_prefers_the_stuck_report_region_then_conversation() {
    let mut with_report = ctx(&[("conversation", 10_000), ("stuck_report", 10_000)]);
    note_stuck(&mut with_report, "implement", "you are looping");
    assert!(
        with_report
            .get_region("stuck_report")
            .unwrap()
            .current_tokens
            > 0
    );
    assert_eq!(
        with_report
            .get_region("conversation")
            .unwrap()
            .current_tokens,
        0
    );

    // Blueprints that declare no stuck_report still get the diagnosis -
    // every blueprint is required to declare `conversation`.
    let mut fallback = ctx(&[("conversation", 10_000)]);
    note_stuck(&mut fallback, "implement", "you are looping");
    let conv = fallback.get_region("conversation").unwrap();
    let text: String = conv.content.iter().map(|e| e.content()).collect();
    assert!(
        text.contains("Stuck detected in stage 'implement'"),
        "{text}"
    );
    assert!(text.contains("you are looping"), "{text}");
}

/// Build a world holding one `ReadyToInfer` agent whose stage `a` carries a
/// `stuck` edge to `b` armed on `cfg`.
fn spawn_stuck_agent(
    world: &mut World,
    cfg: Option<leviath_core::blueprint::StuckConfig>,
    progress: StageProgress,
    status: AgentStatus,
    target_max_revisits: Option<usize>,
    visits: VisitCounts,
) -> Entity {
    let edges = cfg.map(|cfg| {
        let mut e = conditioned_edge("b", TransitionCondition::Stuck);
        e.stuck = Some(cfg);
        vec![("b".to_string(), e)]
    });
    let a = stage_named("a", edges, false, None);
    let b = stage_named("b", None, false, target_max_revisits);
    world
        .spawn((
            AgentBlueprint(blueprint(vec![a, b])),
            StageCursor { index: 0 },
            AgentState {
                status,
                ..agent_state()
            },
            progress,
            visits,
            ctx(&[("conversation", 10_000)]),
            ReadyToInfer,
        ))
        .id()
}

/// The reason carried by a `Stuck` outcome, or `None` for any other (or
/// absent) outcome.
fn stuck_reason_of(outcome: Option<&StageOutcome>) -> Option<&str> {
    match outcome {
        Some(StageOutcome::Stuck(reason)) => Some(reason),
        _ => None,
    }
}

fn run_detect_stuck(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(detect_stuck_stage);
    s.run(world);
}

#[test]
fn detect_stuck_stage_fires_once_and_routes_to_resolve_transition() {
    let mut world = World::new();
    let e = spawn_stuck_agent(
        &mut world,
        Some(stuck_cfg(None, None, Some(3), None)),
        StageProgress {
            edits_by_path: edits(&[("where.py", 3)]),
            ..Default::default()
        },
        AgentStatus::Active,
        Some(2),
        VisitCounts::default(),
    );
    // Opt this agent into a stage log; agents without one (test worlds,
    // `lev run`) still fire, they just don't get the operator line.
    world.entity_mut(e).insert(StageIoBuffer::default());
    run_detect_stuck(&mut world);

    assert!(world.get::<ReadyToInfer>(e).is_none());
    assert!(world.get::<ResolveTransition>(e).is_some());
    let reason = stuck_reason_of(world.get::<StageOutcome>(e)).expect("a Stuck outcome");
    assert!(reason.contains("where.py"), "got: {reason}");
    // The operator sees why, in the stage log the dashboard renders.
    let logs = &world.get::<StageIoBuffer>(e).unwrap().logs;
    assert!(
        logs.iter().any(|(_, line)| line.starts_with("[stuck]")),
        "expected a [stuck] log line, got: {logs:?}"
    );
    // The diagnosis is in context for the stage that has to act on it.
    let window = world.get::<ContextWindow>(e).unwrap();
    let conv = window.get_region("conversation").unwrap();
    assert!(
        conv.content
            .iter()
            .any(|c| c.content().contains("where.py")),
        "the diagnosis must reach the next stage's context"
    );
    assert!(world.get::<StageProgress>(e).unwrap().stuck_fired);

    // One-shot: re-arming the agent must not fire a second time, which is
    // what stops a ping-pong with resolve_transition's resume arm.
    world.entity_mut(e).insert(ReadyToInfer);
    world.entity_mut(e).remove::<ResolveTransition>();
    run_detect_stuck(&mut world);
    assert!(world.get::<ResolveTransition>(e).is_none());
}

#[test]
fn detect_stuck_stage_stamps_the_stage_clock_on_first_sight() {
    let mut world = World::new();
    // Armed on wall clock only: the lazy stamp means turn zero is 0 seconds
    // in, so a fresh agent must NOT trip.
    let e = spawn_stuck_agent(
        &mut world,
        Some(stuck_cfg(None, Some(10), None, None)),
        StageProgress::default(),
        AgentStatus::Active,
        Some(2),
        VisitCounts::default(),
    );
    assert!(
        world
            .get::<StageProgress>(e)
            .unwrap()
            .stage_started_at
            .is_none()
    );
    run_detect_stuck(&mut world);
    assert!(
        world
            .get::<StageProgress>(e)
            .unwrap()
            .stage_started_at
            .is_some()
    );
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<ResolveTransition>(e).is_none());

    // Backdate the stamp past the threshold and it trips.
    let mut progress = world.get_mut::<StageProgress>(e).unwrap();
    progress.stage_started_at = Some(chrono::Utc::now().timestamp() - 11 * 60);
    run_detect_stuck(&mut world);
    let reason = stuck_reason_of(world.get::<StageOutcome>(e)).expect("a Stuck outcome");
    assert!(reason.contains("minutes"), "got: {reason}");
}

#[test]
fn detect_stuck_stage_is_a_noop_without_an_available_stuck_edge() {
    let mut world = World::new();
    let hot = || StageProgress {
        iterations: 99,
        edits_by_path: edits(&[("a.rs", 99)]),
        ..Default::default()
    };
    let cfg = || Some(stuck_cfg(Some(1), None, Some(1), None));

    // (a) the stage declares no stuck edge at all.
    let no_edge = spawn_stuck_agent(
        &mut world,
        None,
        hot(),
        AgentStatus::Active,
        Some(2),
        VisitCounts::default(),
    );
    // (b) the agent is paused/waiting rather than actively working.
    let paused = spawn_stuck_agent(
        &mut world,
        cfg(),
        hot(),
        AgentStatus::Idle,
        Some(2),
        VisitCounts::default(),
    );
    // (c) the escape hatch is spent - the agent must keep working the stage
    //     (bounded by max_iterations) rather than be kicked out elsewhere.
    let mut spent = VisitCounts::default();
    spent.0.insert("b".to_string(), 5);
    let exhausted = spawn_stuck_agent(
        &mut world,
        cfg(),
        hot(),
        AgentStatus::Active,
        Some(2),
        spent,
    );

    run_detect_stuck(&mut world);
    for e in [no_edge, paused, exhausted] {
        assert!(world.get::<ReadyToInfer>(e).is_some());
        assert!(world.get::<ResolveTransition>(e).is_none());
        assert!(stuck_reason_of(world.get::<StageOutcome>(e)).is_none());
        assert!(!world.get::<StageProgress>(e).unwrap().stuck_fired);
    }
}

#[test]
fn find_conditioned_edge_matches_condition_target_and_budget() {
    let err = conditioned_edge("recovery", TransitionCondition::Error);
    let a = stage_named("a", Some(vec![("e".to_string(), err)]), false, None);
    let recovery = stage_named("recovery", None, false, None);
    let bp = blueprint(vec![a, recovery]);
    let visits = std::collections::HashMap::new();
    assert_eq!(
        find_conditioned_edge(&bp, &bp.stages[0], &visits, TransitionCondition::Error)
            .map(|(i, _)| i),
        Some(1)
    );
    // No max_iterations edge present.
    assert!(
        find_conditioned_edge(
            &bp,
            &bp.stages[0],
            &visits,
            TransitionCondition::MaxIterations
        )
        .is_none()
    );
    // A stage with no transitions at all yields nothing.
    let none_bp = blueprint(vec![stage_named("solo", None, false, None)]);
    assert!(
        find_conditioned_edge(
            &none_bp,
            &none_bp.stages[0],
            &visits,
            TransitionCondition::Error
        )
        .is_none()
    );
}

#[test]
fn find_conditioned_edge_skips_unknown_target_and_exhausted_revisits() {
    let ghost = conditioned_edge("nope", TransitionCondition::Error);
    let a = stage_named("a", Some(vec![("g".to_string(), ghost)]), false, None);
    let bp = blueprint(vec![a]);
    let visits = std::collections::HashMap::new();
    assert!(
        find_conditioned_edge(&bp, &bp.stages[0], &visits, TransitionCondition::Error).is_none()
    );

    // Target exists but its revisit budget is exhausted.
    let err = conditioned_edge("recovery", TransitionCondition::Error);
    let a2 = stage_named("a", Some(vec![("e".to_string(), err)]), false, None);
    let recovery = stage_named("recovery", None, false, Some(0));
    let bp2 = blueprint(vec![a2, recovery]);
    let mut visited = std::collections::HashMap::new();
    visited.insert("recovery".to_string(), 1);
    assert!(
        find_conditioned_edge(&bp2, &bp2.stages[0], &visited, TransitionCondition::Error).is_none()
    );
}

fn spawn_outcome_agent(
    world: &mut World,
    bp: leviath_core::Blueprint,
    outcome: StageOutcome,
    status: AgentStatus,
) -> Entity {
    let n = bp.stages.len();
    let infs: Vec<StageInference> = (0..n).map(|_| stage("m", vec![], None)).collect();
    let e = spawn_transition_agent(world, bp, infs, VisitCounts::default());
    world
        .entity_mut(e)
        .insert(outcome)
        .get_mut::<AgentState>()
        .unwrap()
        .status = status;
    e
}

#[test]
fn resolve_transition_routes_error_to_error_edge() {
    let err = conditioned_edge("recovery", TransitionCondition::Error);
    let a = stage_named("a", Some(vec![("e".to_string(), err)]), false, None);
    let recovery = stage_named("recovery", None, false, None);
    let bp = blueprint(vec![a, recovery]);
    let mut world = World::new();
    let e = spawn_outcome_agent(
        &mut world,
        bp,
        StageOutcome::Errored("boom".to_string()),
        AgentStatus::Error {
            message: "boom".to_string(),
        },
    );
    run_transition(&mut world);
    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1); // entered recovery
    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Active
    );
    assert!(world.get::<StageOutcome>(e).is_none());
}

#[test]
fn resolve_transition_errors_terminally_without_an_error_edge() {
    // Stage 'a' has only an Always edge to 'b' - no error edge.
    let a = stage_named(
        "a",
        Some(vec![("go".to_string(), plain_edge("b"))]),
        false,
        None,
    );
    let b = stage_named("b", None, false, None);
    let bp = blueprint(vec![a, b]);
    let mut world = World::new();
    let e = spawn_outcome_agent(
        &mut world,
        bp,
        StageOutcome::Errored("boom".to_string()),
        AgentStatus::Error {
            message: "boom".to_string(),
        },
    );
    run_transition(&mut world);
    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 0); // no transition
    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Error {
            message: "boom".to_string()
        }
    );
    assert!(world.get::<StageOutcome>(e).is_none());
    assert!(world.get::<ResolveTransition>(e).is_none());
}

#[test]
fn resolve_transition_routes_max_iterations_edge_else_falls_through() {
    // With a max_iterations edge → follow it.
    let mi = conditioned_edge("recovery", TransitionCondition::MaxIterations);
    let a = stage_named("a", Some(vec![("m".to_string(), mi)]), false, None);
    let recovery = stage_named("recovery", None, false, None);
    let bp = blueprint(vec![a, recovery]);
    let mut world = World::new();
    let e = spawn_outcome_agent(
        &mut world,
        bp,
        StageOutcome::MaxIterations,
        AgentStatus::Active,
    );
    run_transition(&mut world);
    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);

    // Without one → fall through to a normal (linear) transition.
    let a2 = stage_named("a", None, false, None);
    let b2 = stage_named("b", None, false, None);
    let bp2 = blueprint(vec![a2, b2]);
    let mut world2 = World::new();
    let e2 = spawn_outcome_agent(
        &mut world2,
        bp2,
        StageOutcome::MaxIterations,
        AgentStatus::Active,
    );
    run_transition(&mut world2);
    assert_eq!(world2.get::<StageCursor>(e2).unwrap().index, 1); // linear fall-through
    assert!(world2.get::<StageOutcome>(e2).is_none());
}

#[test]
fn resolve_transition_routes_stuck_down_the_stuck_edge() {
    let mut stuck = conditioned_edge("reassess", TransitionCondition::Stuck);
    stuck.stuck = Some(stuck_cfg(Some(20), None, None, None));
    let a = stage_named("a", Some(vec![("s".to_string(), stuck)]), false, None);
    let reassess = stage_named("reassess", None, false, Some(2));
    let bp = blueprint(vec![a, reassess]);
    let mut world = World::new();
    let e = spawn_outcome_agent(
        &mut world,
        bp,
        StageOutcome::Stuck("looping".to_string()),
        AgentStatus::Active,
    );
    run_transition(&mut world);
    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1); // entered reassess
    assert!(world.get::<StageOutcome>(e).is_none());
}

/// A stuck interrupt fires MID-stage, so when its escape edge is gone the
/// agent must go back to work - falling through to a normal transition
/// would end a stage the agent never said it had finished (e.g. shunting
/// `implement` into `review` with the work half-done).
#[test]
fn resolve_transition_resumes_the_stage_when_the_stuck_edge_is_gone() {
    // Stage 'a' has only an ordinary edge to 'b' - no stuck edge at all,
    // which is what an exhausted revisit budget looks like from here.
    let a = stage_named(
        "a",
        Some(vec![("n".to_string(), plain_edge("b"))]),
        false,
        None,
    );
    let b = stage_named("b", None, false, None);
    let bp = blueprint(vec![a, b]);
    let mut world = World::new();
    let e = spawn_outcome_agent(
        &mut world,
        bp,
        StageOutcome::Stuck("looping".to_string()),
        AgentStatus::Active,
    );
    run_transition(&mut world);

    assert_eq!(
        world.get::<StageCursor>(e).unwrap().index,
        0,
        "the agent must stay in its current stage"
    );
    assert!(
        world.get::<ReadyToInfer>(e).is_some(),
        "and go back to work"
    );
    assert!(world.get::<ResolveTransition>(e).is_none());
    assert!(world.get::<StageOutcome>(e).is_none());
}

// ── required-region gating (#5) ──

fn required_bp(tools: &[&str], custom_msg: Option<&str>) -> AgentBlueprint {
    let region =
        leviath_core::layout::RegionDefinition::new("plan".to_string(), RegionKind::Pinned, 4000)
            .with_required(true, custom_msg.map(str::to_string));
    let layout = leviath_core::layout::ContextLayout::new(vec![region], 10_000);
    let mut stage = stage_named("a", None, false, None);
    stage.available_tools = tools.iter().map(|s| s.to_string()).collect();
    stage.context_layout = Some(layout.clone());
    AgentBlueprint(leviath_core::Blueprint::new(
        "t".to_string(),
        "d".to_string(),
        vec![stage],
        layout,
    ))
}

fn window_with_plan(filled: bool) -> ContextWindow {
    let mut w = ContextWindow::new(100_000);
    w.add_region(Region::new("plan".to_string(), RegionKind::Pinned, 4000));
    w.add_region(Region::new(
        "conversation".to_string(),
        RegionKind::Clearable,
        10_000,
    ));
    if filled {
        w.add_to_region("plan", "the plan".to_string(), 5).unwrap();
    }
    w
}

#[test]
fn unmet_required_regions_flags_empty_clears_when_filled_and_skips_without_tool() {
    let bp = required_bp(&["context_write"], None);
    assert_eq!(
        unmet_required_regions(&bp.0, &bp.0.stages[0], &window_with_plan(false)).len(),
        1
    );
    assert!(unmet_required_regions(&bp.0, &bp.0.stages[0], &window_with_plan(true)).is_empty());
    // No context-writing tool ⇒ never gated (would loop pointlessly).
    let no_tool = required_bp(&["read_file"], None);
    assert!(
        unmet_required_regions(&no_tool.0, &no_tool.0.stages[0], &window_with_plan(false))
            .is_empty()
    );
    // A required region absent from the window entirely counts as unmet.
    let mut bare = ContextWindow::new(100_000);
    bare.add_region(Region::new(
        "conversation".to_string(),
        RegionKind::Clearable,
        10_000,
    ));
    assert_eq!(
        unmet_required_regions(&bp.0, &bp.0.stages[0], &bare).len(),
        1
    );
}

#[test]
fn unmet_required_regions_skips_caller_input_seeded_regions() {
    // A required region whose content comes from the caller at spawn must NOT
    // be flagged by the agent-facing gate, even when empty and the stage can
    // write context - the caller owns it, not the agent.
    let region =
        leviath_core::layout::RegionDefinition::new("plan".to_string(), RegionKind::Pinned, 4000)
            .with_required(true, None)
            .with_seed(leviath_core::layout::RegionSeed::CallerInput {
                name: "plan".to_string(),
            });
    let layout = leviath_core::layout::ContextLayout::new(vec![region], 10_000);
    let mut stage = stage_named("a", None, false, None);
    stage.available_tools = vec!["context_write".to_string()];
    stage.context_layout = Some(layout.clone());
    let bp = AgentBlueprint(leviath_core::Blueprint::new(
        "t".to_string(),
        "d".to_string(),
        vec![stage],
        layout,
    ));
    assert!(
        unmet_required_regions(&bp.0, &bp.0.stages[0], &window_with_plan(false)).is_empty(),
        "caller-input region is validated at spawn, not gated here"
    );
}

#[test]
fn unmet_required_regions_falls_back_to_blueprint_layout() {
    // The stage has no per-stage layout, so the blueprint's layout is used.
    let mut bp = required_bp(&["context_write"], None);
    bp.0.stages[0].context_layout = None;
    assert_eq!(
        unmet_required_regions(&bp.0, &bp.0.stages[0], &window_with_plan(false)).len(),
        1
    );
}

fn run_require(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(require_context_regions);
    s.run(world);
}

#[test]
fn require_context_regions_reruns_stage_on_unmet() {
    let mut world = World::new();
    let e = world
        .spawn((
            required_bp(&["context_write"], Some("write the plan!")),
            StageCursor { index: 0 },
            window_with_plan(false),
            ResolveTransition,
        ))
        .id();
    run_require(&mut world);
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<ResolveTransition>(e).is_none());
    assert_eq!(world.get::<RequiredReentries>(e).unwrap().0, 1);
    // The custom nudge was injected into conversation.
    assert!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .current_tokens
            > 0
    );
}

#[test]
fn require_context_regions_injects_default_message() {
    // No custom required_message ⇒ the default nudge text is used.
    let mut world = World::new();
    let e = world
        .spawn((
            required_bp(&["context_write"], None),
            StageCursor { index: 0 },
            window_with_plan(false),
            ResolveTransition,
        ))
        .id();
    run_require(&mut world);
    let conv = world
        .get::<ContextWindow>(e)
        .unwrap()
        .get_region("conversation")
        .unwrap()
        .content
        .iter()
        .map(|entry| entry.content_owned())
        .collect::<String>();
    assert!(conv.contains("Required context region 'plan' is still empty"));
}

#[test]
fn require_context_regions_proceeds_when_met_capped_or_errored() {
    let mut world = World::new();
    // met ⇒ proceed
    let met = world
        .spawn((
            required_bp(&["context_write"], None),
            StageCursor { index: 0 },
            window_with_plan(true),
            ResolveTransition,
        ))
        .id();
    // unmet but at the cap ⇒ proceed with a warning
    let capped = world
        .spawn((
            required_bp(&["context_write"], None),
            StageCursor { index: 0 },
            window_with_plan(false),
            RequiredReentries(DEFAULT_REQUIRED_REENTRY_CAP),
            ResolveTransition,
        ))
        .id();
    // unmet but the stage errored ⇒ the error transition takes precedence
    let errored = world
        .spawn((
            required_bp(&["context_write"], None),
            StageCursor { index: 0 },
            window_with_plan(false),
            StageOutcome::Errored("boom".to_string()),
            ResolveTransition,
        ))
        .id();
    run_require(&mut world);
    for e in [met, capped, errored] {
        assert!(world.get::<ResolveTransition>(e).is_some());
        assert!(world.get::<ReadyToInfer>(e).is_none());
    }
}

// ── transition gates: require_modifications (#107) ──

fn gate(region: Option<&str>, message: Option<&str>) -> leviath_core::blueprint::TransitionGate {
    leviath_core::blueprint::TransitionGate {
        require_modifications: true,
        message: message.map(str::to_string),
        region: region.map(str::to_string),
        tools: Vec::new(),
        max_attempts: None,
    }
}

/// A stage that can write files, with `edges` attached.
fn writing_stage(
    name: &str,
    edges: Vec<(String, leviath_core::blueprint::TransitionEdge)>,
) -> leviath_core::Stage {
    let mut s = stage_named(name, Some(edges), false, None);
    s.available_tools = vec!["write_file".to_string(), "bash".to_string()];
    s
}

fn gated_edge(
    target: &str,
    gate: Option<leviath_core::blueprint::TransitionGate>,
) -> (String, leviath_core::blueprint::TransitionEdge) {
    (
        target.to_string(),
        leviath_core::blueprint::TransitionEdge {
            target: target.to_string(),
            condition: leviath_core::blueprint::TransitionCondition::Always,
            hint: None,
            transform: leviath_core::blueprint::EdgeTransform::Direct,
            gate,
            stuck: None,
        },
    )
}

/// The nudge a gate would show, or `None` when it let the transition
/// through. A named helper rather than an inline `matches!` so both arms are
/// exercised by the assertions below.
fn block_message(decision: GateDecision) -> Option<String> {
    match decision {
        GateDecision::Block(msg) => Some(msg),
        GateDecision::Pass | GateDecision::Forced => None,
    }
}

fn progress_with(modifying: usize, blocked: usize, reentries: usize) -> StageProgress {
    StageProgress {
        modifying_tool_calls: modifying,
        blocked_modification_calls: blocked,
        gate_reentries: reentries,
        ..Default::default()
    }
}

#[test]
fn gate_blocks_only_an_unsatisfied_require_modifications_edge() {
    let stage = writing_stage("impl", vec![gated_edge("review", Some(gate(None, None)))]);
    let window = conv_window();
    let zero = progress_with(0, 0, 0);
    // Unsatisfied ⇒ blocked, with the default explanation.
    let g = gate(None, None);
    let msg = block_message(gate_blocks(Some(&g), &stage, &zero, &window))
        .expect("an unsatisfied require_modifications gate blocks");
    assert!(msg.contains("edit_file or write_file"));
    // No gate at all, and a gate that doesn't require modifications, both pass.
    assert_eq!(
        gate_blocks(None, &stage, &zero, &window),
        GateDecision::Pass
    );
    let off = leviath_core::blueprint::TransitionGate::default();
    assert_eq!(
        gate_blocks(Some(&off), &stage, &zero, &window),
        GateDecision::Pass
    );
    // A landed write satisfies it.
    assert_eq!(
        gate_blocks(Some(&g), &stage, &progress_with(1, 0, 0), &window),
        GateDecision::Pass
    );
    // So does a write the permission layer refused: the agent is trying and
    // cannot, so another pass would only burn iterations.
    assert_eq!(
        gate_blocks(Some(&g), &stage, &progress_with(0, 1, 0), &window),
        GateDecision::Pass
    );
}

#[test]
fn gate_uses_a_custom_message_when_given() {
    let stage = writing_stage("impl", vec![]);
    let g = gate(None, Some("write something!"));
    assert_eq!(
        gate_blocks(Some(&g), &stage, &progress_with(0, 0, 0), &conv_window()),
        GateDecision::Block("write something!".to_string())
    );
}

#[test]
fn gate_passes_on_a_non_empty_evidence_region() {
    // The resume case: per-stage counters are gone after a daemon restart,
    // but the region the write tools are routed into is restored from disk.
    let stage = writing_stage("impl", vec![]);
    let g = gate(Some("implementation"), None);
    let zero = progress_with(0, 0, 0);

    let mut empty = conv_window();
    empty.add_region(Region::new(
        "implementation".to_string(),
        RegionKind::Clearable,
        10_000,
    ));
    // Region present but empty ⇒ still gated; region missing entirely ⇒ gated.
    assert!(block_message(gate_blocks(Some(&g), &stage, &zero, &empty)).is_some());
    assert!(block_message(gate_blocks(Some(&g), &stage, &zero, &conv_window())).is_some());

    let mut filled = empty.clone();
    filled
        .add_to_region("implementation", "wrote src/lib.rs".to_string(), 5)
        .unwrap();
    assert!(block_message(gate_blocks(Some(&g), &stage, &zero, &filled)).is_none());
}

#[test]
fn gate_passes_a_stage_that_cannot_modify_anything() {
    // Gating a stage with no write tool would loop pointlessly; the blueprint
    // validator rejects that combination, but the runtime never relies on it.
    let mut stage = writing_stage("review", vec![]);
    stage.available_tools = vec!["read_file".to_string()];
    let g = gate(None, None);
    assert_eq!(
        gate_blocks(Some(&g), &stage, &progress_with(0, 0, 0), &conv_window()),
        GateDecision::Pass
    );
    // ...unless the gate itself names the tool the stage does have.
    let mut custom = gate(None, None);
    custom.tools = vec!["read_file".to_string()];
    assert!(
        block_message(gate_blocks(
            Some(&custom),
            &stage,
            &progress_with(0, 0, 0),
            &conv_window()
        ))
        .is_some()
    );
}

#[test]
fn gate_gives_up_after_its_attempt_budget() {
    let stage = writing_stage("impl", vec![]);
    let zero_window = conv_window();
    // Default budget is 3 re-runs.
    let g = gate(None, None);
    assert!(
        block_message(gate_blocks(
            Some(&g),
            &stage,
            &progress_with(0, 0, 2),
            &zero_window
        ))
        .is_some()
    );
    assert_eq!(
        gate_blocks(Some(&g), &stage, &progress_with(0, 0, 3), &zero_window),
        GateDecision::Forced
    );
    // ...and is overridable per edge.
    let mut once = gate(None, None);
    once.max_attempts = Some(1);
    assert_eq!(
        gate_blocks(Some(&once), &stage, &progress_with(0, 0, 1), &zero_window),
        GateDecision::Forced
    );
}

#[test]
fn resolve_transition_holds_the_stage_when_a_gate_blocks() {
    let bp = blueprint(vec![
        writing_stage("impl", vec![gated_edge("review", Some(gate(None, None)))]),
        stage_named("review", None, false, None),
    ]);
    let mut world = World::new();
    let e = spawn_transition_agent(
        &mut world,
        bp,
        vec![si("m0"), si("m1")],
        VisitCounts::default(),
    );
    world
        .entity_mut(e)
        .insert(progress_with(0, 0, 0))
        .insert(crate::persistence::RunOutcomeFlags::default());

    run_transition(&mut world);

    // Still in `impl`, re-armed for another inference, nudged, and counted.
    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 0);
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<ResolveTransition>(e).is_none());
    assert_eq!(world.get::<StageProgress>(e).unwrap().gate_reentries, 1);
    let conv = world
        .get::<ContextWindow>(e)
        .unwrap()
        .get_region("conversation")
        .unwrap()
        .content
        .iter()
        .map(|entry| entry.content_owned())
        .collect::<String>();
    assert!(conv.contains("[System] No file modifications"));
    // Not yet forced - the budget hasn't run out.
    assert_eq!(
        world
            .get::<crate::persistence::RunOutcomeFlags>(e)
            .unwrap()
            .0
            .gates_forced,
        0
    );
}

#[test]
fn resolve_transition_records_a_forced_gate_and_advances() {
    let bp = blueprint(vec![
        writing_stage("impl", vec![gated_edge("review", Some(gate(None, None)))]),
        stage_named("review", None, false, None),
    ]);
    let mut world = World::new();
    let e = spawn_transition_agent(
        &mut world,
        bp.clone(),
        vec![si("m0"), si("m1")],
        VisitCounts::default(),
    );
    world
        .entity_mut(e)
        // Budget already spent.
        .insert(progress_with(0, 0, 3))
        .insert(crate::persistence::RunOutcomeFlags::default());
    // An agent with no flags component (fan-out workers, older runs) still
    // transitions - it just has nowhere to record the forced gate.
    let unflagged = spawn_transition_agent(
        &mut world,
        bp,
        vec![si("m0"), si("m1")],
        VisitCounts::default(),
    );
    world.entity_mut(unflagged).insert(progress_with(0, 0, 3));

    run_transition(&mut world);

    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    assert_eq!(world.get::<StageCursor>(unflagged).unwrap().index, 1);
    assert_eq!(
        world
            .get::<crate::persistence::RunOutcomeFlags>(e)
            .unwrap()
            .0
            .gates_forced,
        1
    );
}

#[test]
fn resolve_transition_skips_the_gate_on_an_error_edge() {
    use leviath_core::blueprint::TransitionCondition;
    // The error edge is followed even with zero modifications: a failed stage
    // must be able to reach recovery.
    let mut error_edge = gated_edge("recover", Some(gate(None, None)));
    error_edge.1.condition = TransitionCondition::Error;
    let bp = blueprint(vec![
        writing_stage("impl", vec![error_edge]),
        stage_named("recover", None, false, None),
    ]);
    let mut world = World::new();
    let e = spawn_transition_agent(
        &mut world,
        bp,
        vec![si("m0"), si("m1")],
        VisitCounts::default(),
    );
    world
        .entity_mut(e)
        .insert(progress_with(0, 0, 0))
        .insert(StageOutcome::Errored("boom".to_string()));

    run_transition(&mut world);

    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    assert_eq!(world.get::<StageProgress>(e).unwrap().gate_reentries, 0);
}

// ── file tracking (#6) ──

fn ftc(
    reads: bool,
    writes: bool,
    max: Option<usize>,
) -> leviath_core::blueprint::FileTrackingConfig {
    leviath_core::blueprint::FileTrackingConfig {
        region: "files".to_string(),
        track_reads: reads,
        track_writes: writes,
        max_file_tokens: max,
    }
}

fn fcall(id: &str, name: &str, args: serde_json::Value) -> crate::components::ToolCall {
    crate::components::ToolCall {
        tool_id: id.to_string(),
        name: name.to_string(),
        arguments: args,
        thought_signature: None,
    }
}

fn hashmap_window() -> ContextWindow {
    let mut w = ContextWindow::new(100_000);
    w.add_region(Region::new(
        "files".to_string(),
        RegionKind::HashMap { max_entries: None },
        40_000,
    ));
    w
}

#[test]
fn truncate_file_caps_only_when_over_the_limit() {
    assert_eq!(truncate_file("short".to_string(), Some(100)), "short");
    assert_eq!(truncate_file("short".to_string(), None), "short");
    let out = truncate_file("x".repeat(500), Some(10)); // 10*4 = 40 chars
    assert!(out.contains("truncated at 10 tokens"));
    assert!(out.len() < 500);
}

#[test]
fn apply_file_tracking_tracks_reads_and_writes() {
    let ft = ftc(true, true, Some(2)); // small cap to also exercise truncation
    let mut w = hashmap_window();
    let calls = vec![
        fcall("1", "read_file", serde_json::json!({"path": "a.rs"})),
        fcall(
            "2",
            "write_file",
            serde_json::json!({"path": "b.rs", "content": "fn b() {}"}),
        ),
    ];
    let mut merged = vec![
        ("1".to_string(), "fn a() { /* long body */ }".to_string()),
        ("2".to_string(), "written ok".to_string()),
    ];
    apply_file_tracking(&mut w, &ft, &calls, &mut merged);
    assert!(merged[0].1.contains("Reference it there"));
    assert!(merged[1].1.contains("Reference it there"));
    assert_eq!(w.get_region("files").unwrap().content.len(), 2);
}

#[test]
fn apply_file_tracking_noop_without_a_hashmap_region() {
    let ft = ftc(true, true, None);
    let calls = vec![fcall("1", "read_file", serde_json::json!({"path": "a"}))];
    let mut merged = vec![("1".to_string(), "body".to_string())];
    // No "files" region at all.
    let mut w1 = ContextWindow::new(100_000);
    apply_file_tracking(&mut w1, &ft, &calls, &mut merged);
    assert_eq!(merged[0].1, "body");
    // "files" region exists but isn't a HashMap.
    let mut w2 = ContextWindow::new(100_000);
    w2.add_region(Region::new(
        "files".to_string(),
        RegionKind::Clearable,
        40_000,
    ));
    apply_file_tracking(&mut w2, &ft, &calls, &mut merged);
    assert_eq!(merged[0].1, "body");
}

#[test]
fn apply_file_tracking_skips_errors_missing_path_other_tools_and_flags() {
    let mut w = hashmap_window();
    let ft = ftc(true, true, None);
    let calls = vec![
        fcall("1", "read_file", serde_json::json!({"path": "a"})), // result is an error
        fcall("2", "read_file", serde_json::json!({})),            // no path
        fcall("3", "list_dir", serde_json::json!({"path": "d"})),  // untracked tool
        fcall("4", "write_file", serde_json::json!({"path": "e"})), // no content
        fcall("5", "read_file", serde_json::json!({"path": "f"})), // result is denied
        // Never offered by this stage: the write did not happen, so tracking
        // it would put a file in the region that does not exist on disk.
        fcall(
            "6",
            "write_file",
            serde_json::json!({"path": "g", "content": "print(1)"}),
        ),
    ];
    let mut merged = vec![
        ("1".to_string(), "[error] boom".to_string()),
        ("2".to_string(), "body".to_string()),
        ("3".to_string(), "listing".to_string()),
        ("4".to_string(), "written".to_string()),
        ("5".to_string(), "[denied] nope".to_string()),
        (
            "6".to_string(),
            "[unavailable] 'write_file' is not available in this stage.".to_string(),
        ),
    ];
    apply_file_tracking(&mut w, &ft, &calls, &mut merged);
    for (_, r) in &merged {
        assert!(!r.contains("Reference it there"));
    }
    assert_eq!(w.get_region("files").unwrap().content.len(), 0);

    // With tracking flags off, read/write are also skipped.
    let off = ftc(false, false, None);
    let calls2 = vec![
        fcall("1", "read_file", serde_json::json!({"path": "a"})),
        fcall(
            "2",
            "write_file",
            serde_json::json!({"path": "b", "content": "x"}),
        ),
    ];
    let mut merged2 = vec![
        ("1".to_string(), "body".to_string()),
        ("2".to_string(), "written".to_string()),
    ];
    apply_file_tracking(&mut w, &off, &calls2, &mut merged2);
    for (_, r) in &merged2 {
        assert!(!r.contains("Reference it there"));
    }
}

#[test]
fn collect_tools_applies_file_tracking_from_blueprint() {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolResults(rx));
    let mut w = hashmap_window();
    w.add_region(Region::new(
        "conversation".to_string(),
        RegionKind::Clearable,
        10_000,
    ));
    // A blueprint carrying a file_tracking config.
    let layout = leviath_core::layout::ContextLayout::new(vec![], 10_000);
    let mut bp = leviath_core::Blueprint::new(
        "t".to_string(),
        "d".to_string(),
        vec![stage_named("a", None, false, None)],
        layout,
    );
    bp.file_tracking = Some(ftc(true, true, None));
    let e = world
        .spawn((
            w,
            infer_with(vec![fcall(
                "c1",
                "read_file",
                serde_json::json!({"path": "a.rs"}),
            )]),
            AwaitingTools,
            AgentBlueprint(bp),
        ))
        .id();
    tx.send(ToolOutcome {
        elapsed: std::time::Duration::ZERO,
        entity: e,
        results: vec![("c1".to_string(), "fn a() {}".to_string())],
    })
    .unwrap();
    run_collect_tools(&mut world);
    // The file body landed in the HashMap region.
    assert_eq!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("files")
            .unwrap()
            .content
            .len(),
        1
    );
}

// ── modification accounting (#107) ──

/// Drive `collect_tools` over one batch of `(tool, result)` pairs against a
/// stage whose outgoing edge names `extra_tools` as modifying, returning the
/// resulting per-stage progress and run flags.
fn count_modifications(
    calls: &[(&str, serde_json::Value, &str)],
    extra_tools: &[&str],
) -> (StageProgress, leviath_core::run_meta::RunFlags) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolResults(rx));
    let mut g = gate(None, None);
    g.tools = extra_tools.iter().map(|t| (*t).to_string()).collect();
    let bp = blueprint(vec![writing_stage(
        "impl",
        vec![gated_edge("review", Some(g))],
    )]);
    let e = world
        .spawn((
            conv_window(),
            infer_with(
                calls
                    .iter()
                    .enumerate()
                    .map(|(i, (name, args, _))| fcall(&format!("c{i}"), name, args.clone()))
                    .collect(),
            ),
            AwaitingTools,
            AgentBlueprint(bp),
            StageCursor { index: 0 },
            StageProgress::default(),
            crate::persistence::RunOutcomeFlags::default(),
        ))
        .id();
    tx.send(ToolOutcome {
        elapsed: std::time::Duration::ZERO,
        entity: e,
        results: calls
            .iter()
            .enumerate()
            .map(|(i, (_, _, result))| (format!("c{i}"), (*result).to_string()))
            .collect(),
    })
    .unwrap();
    run_collect_tools(&mut world);
    (
        world.get::<StageProgress>(e).unwrap().clone(),
        world
            .get::<crate::persistence::RunOutcomeFlags>(e)
            .unwrap()
            .0
            .clone(),
    )
}

#[test]
fn collect_tools_counts_successful_writes_and_their_paths() {
    let (progress, flags) = count_modifications(
        &[
            (
                "write_file",
                serde_json::json!({"path": "src/a.rs"}),
                "Successfully wrote 12 bytes to 'src/a.rs'",
            ),
            (
                "edit_file",
                serde_json::json!({"path": "src/b.rs"}),
                "Successfully edited 'src/b.rs'",
            ),
            // Same path twice: counted twice, listed once.
            (
                "edit_file",
                serde_json::json!({"path": "src/b.rs"}),
                "Successfully edited 'src/b.rs'",
            ),
        ],
        &[],
    );
    assert_eq!(progress.modifying_tool_calls, 3);
    assert_eq!(progress.blocked_modification_calls, 0);
    assert_eq!(flags.modified_file_count, 3);
    assert_eq!(flags.modified_files, vec!["src/a.rs", "src/b.rs"]);
}

#[test]
fn collect_tools_separates_failed_denied_and_non_modifying_calls() {
    let (progress, flags) = count_modifications(
        &[
            // Read-only work through the shell is exactly what #107 is about:
            // it must not read as a modification.
            ("shell", serde_json::json!({"command": "cat a.rs"}), "…"),
            (
                "write_file",
                serde_json::json!({"path": "a.rs"}),
                "[error] Failed to write 'a.rs': permission denied",
            ),
            (
                "edit_file",
                serde_json::json!({"path": "b.rs"}),
                "[denied] User declined tool call 'edit_file'.",
            ),
        ],
        &[],
    );
    assert_eq!(progress.modifying_tool_calls, 0);
    assert_eq!(progress.blocked_modification_calls, 1);
    assert_eq!(flags.modified_file_count, 0);
    assert!(flags.modified_files.is_empty());
}

/// A write the stage never offered is not a modification. It matters twice
/// over: `modified_files` in `meta.json` would name a file that was never
/// written, and `modifying_tool_calls` is what a `require_modifications`
/// transition gate reads - so a stage that had every write refused could
/// still answer "yes, I did work" on the way out.
#[test]
fn collect_tools_ignores_a_write_the_stage_never_offered() {
    let (progress, flags) = count_modifications(
        &[
            (
                "write_file",
                serde_json::json!({"path": "smuggled.py"}),
                "[unavailable] 'write_file' is not available in this stage. \
                 You may call: read_file, list_dir.",
            ),
            (
                "edit_file",
                serde_json::json!({"path": "also-not.rs"}),
                "[unavailable] 'edit_file' is not available in this stage.",
            ),
        ],
        &[],
    );
    assert_eq!(progress.modifying_tool_calls, 0);
    // Not "blocked" either - nobody declined it; the stage never had it.
    assert_eq!(progress.blocked_modification_calls, 0);
    assert_eq!(flags.modified_file_count, 0);
    assert!(flags.modified_files.is_empty());
}

#[test]
fn collect_tools_counts_a_gates_extra_tools_by_canonical_name() {
    // `bash` is an alias for `shell`; a gate naming either one counts the
    // canonical tool the agent actually calls.
    let (progress, flags) = count_modifications(
        &[("shell", serde_json::json!({"command": "make"}), "ok")],
        &["bash"],
    );
    assert_eq!(progress.modifying_tool_calls, 1);
    // No `path` argument to record; the count still rises.
    assert_eq!(flags.modified_file_count, 1);
    assert_eq!(flags.modified_files, vec!["<unknown>"]);
}

#[test]
fn collect_tools_still_applies_results_without_stage_components() {
    // Agents spawned without StageProgress/RunOutcomeFlags (fan-out workers
    // mid-setup, and much of this test suite) must not have their tool
    // results silently dropped by the accounting query.
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolResults(rx));
    let e = world
        .spawn((
            conv_window(),
            infer_with(vec![
                fcall("c1", "write_file", serde_json::json!({"path": "a.rs"})),
                fcall("c2", "edit_file", serde_json::json!({"path": "b.rs"})),
            ]),
            AwaitingTools,
        ))
        .id();
    tx.send(ToolOutcome {
        elapsed: std::time::Duration::ZERO,
        entity: e,
        results: vec![
            ("c1".to_string(), "wrote it".to_string()),
            // Both the counted and the blocked path must tolerate the
            // missing components.
            (
                "c2".to_string(),
                "[denied] User declined tool call 'edit_file'.".to_string(),
            ),
        ],
    })
    .unwrap();
    run_collect_tools(&mut world);
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .current_tokens
            > 0
    );
}

#[test]
fn stage_modifying_tools_defaults_without_a_blueprint_or_stage() {
    let defaults = vec!["write_file".to_string(), "edit_file".to_string()];
    // No blueprint / no cursor.
    assert_eq!(stage_modifying_tools(None, None), defaults);
    // A cursor pointing past the end of the blueprint's stages.
    let bp = AgentBlueprint(blueprint(vec![stage_named("a", None, false, None)]));
    assert_eq!(
        stage_modifying_tools(Some(&bp), Some(&StageCursor { index: 9 })),
        defaults
    );
    // A stage with no transitions at all.
    assert_eq!(
        stage_modifying_tools(Some(&bp), Some(&StageCursor { index: 0 })),
        defaults
    );
    // An edge with no gate.
    let ungated = AgentBlueprint(blueprint(vec![writing_stage(
        "a",
        vec![gated_edge("b", None)],
    )]));
    assert_eq!(
        stage_modifying_tools(Some(&ungated), Some(&StageCursor { index: 0 })),
        defaults
    );
    // A gate that re-lists a built-in doesn't duplicate it.
    let mut dup = gate(None, None);
    dup.tools = vec!["write_file".to_string()];
    let deduped = AgentBlueprint(blueprint(vec![writing_stage(
        "a",
        vec![gated_edge("b", Some(dup))],
    )]));
    assert_eq!(
        stage_modifying_tools(Some(&deduped), Some(&StageCursor { index: 0 })),
        defaults
    );
}

// ── workspace health (#107) ──

fn run_workspace_check(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(check_workspace_health);
    s.run(world);
}

fn spawn_workspace_agent(world: &mut World, workdir: &str, iterations: usize) -> Entity {
    let mut md = run_metadata();
    md.workdir = workdir.to_string();
    world
        .spawn((
            md,
            StageProgress {
                iterations,
                ..Default::default()
            },
            agent_state(),
            crate::persistence::RunOutcomeFlags::default(),
            ReadyToInfer,
        ))
        .id()
}

#[test]
fn workspace_check_fails_a_run_whose_directory_is_gone() {
    let mut world = World::new();
    let e = spawn_workspace_agent(&mut world, "/definitely/not/a/real/dir", 0);
    run_workspace_check(&mut world);
    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Error {
            message: "workspace '/definitely/not/a/real/dir' is no longer accessible".to_string()
        }
    );
    assert!(
        world
            .get::<crate::persistence::RunOutcomeFlags>(e)
            .unwrap()
            .0
            .workspace_lost
    );
    assert!(world.get::<ReadyToInfer>(e).is_none());
}

#[test]
fn workspace_check_rejects_a_workdir_that_is_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, "x").unwrap();
    let mut world = World::new();
    let e = spawn_workspace_agent(&mut world, &file.to_string_lossy(), 0);
    run_workspace_check(&mut world);
    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Error {
            message: format!("workspace '{}' is no longer accessible", file.display())
        }
    );
}

#[test]
fn workspace_check_is_a_no_op_when_healthy_off_interval_or_inactive() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().to_string_lossy().to_string();
    let mut world = World::new();
    // Healthy workspace.
    let healthy = spawn_workspace_agent(&mut world, &live, 0);
    // Missing workspace, but this iteration isn't a check point.
    let off_interval = spawn_workspace_agent(&mut world, "/gone", 1);
    // Missing workspace, but the agent isn't running.
    let idle = spawn_workspace_agent(&mut world, "/gone", 0);
    world.get_mut::<AgentState>(idle).unwrap().status = AgentStatus::Waiting;

    run_workspace_check(&mut world);

    assert_eq!(
        world.get::<AgentState>(healthy).unwrap().status,
        AgentStatus::Active
    );
    assert_eq!(
        world.get::<AgentState>(off_interval).unwrap().status,
        AgentStatus::Active
    );
    assert_eq!(
        world.get::<AgentState>(idle).unwrap().status,
        AgentStatus::Waiting
    );
    for e in [healthy, off_interval, idle] {
        assert!(world.get::<ReadyToInfer>(e).is_some());
    }
}

// ── repetition detection (#8) ──

#[test]
fn collect_tools_injects_repetition_nudge_when_looping() {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolResults(rx));
    let e = world
        .spawn((
            ctx(&[("conversation", 10_000)]),
            // Two identical read_file calls (args are Null for both).
            infer_with(vec![tc("c1", "read_file"), tc("c2", "read_file")]),
            AwaitingTools,
            crate::repetition::RepetitionDetector::new(crate::repetition::RepetitionConfig {
                max_repeat_calls: 1,
                max_readonly_streak: 100,
                enabled: true,
            }),
        ))
        .id();
    tx.send(ToolOutcome {
        elapsed: std::time::Duration::ZERO,
        entity: e,
        results: vec![
            ("c1".to_string(), "body".to_string()),
            ("c2".to_string(), "body".to_string()),
        ],
    })
    .unwrap();
    run_collect_tools(&mut world);
    let joined: String = world
        .get::<ContextWindow>(e)
        .unwrap()
        .get_region("conversation")
        .unwrap()
        .content
        .iter()
        .map(|entry| entry.content_owned())
        .collect();
    assert!(
        joined.contains("[System]"),
        "expected a nudge, got: {joined}"
    );
}

// ── requires_children gate (#7) ──

use crate::components::SubAgentChildren;

fn state_with(status: AgentStatus) -> AgentState {
    AgentState {
        status,
        ..agent_state()
    }
}

fn requires_children_bp(req: bool) -> AgentBlueprint {
    let mut s = stage_named("a", None, false, None);
    s.requires_children = req;
    AgentBlueprint(blueprint(vec![s]))
}

fn children(entities: Vec<Entity>) -> SubAgentChildren {
    SubAgentChildren {
        children: entities,
        max_child_depth: 3,
    }
}

fn run_gate_children(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(gate_requires_children);
    s.run(world);
}

#[test]
fn is_terminal_status_classifies_all_variants() {
    assert!(is_terminal_status(&AgentStatus::Complete));
    assert!(is_terminal_status(&AgentStatus::Error {
        message: "x".to_string()
    }));
    assert!(is_terminal_status(&AgentStatus::Cancelled));
    assert!(!is_terminal_status(&AgentStatus::Active));
    assert!(!is_terminal_status(&AgentStatus::Idle));
    assert!(!is_terminal_status(&AgentStatus::Waiting));
}

#[test]
fn gate_requires_children_holds_then_resumes() {
    let mut world = World::new();
    let child = world.spawn(state_with(AgentStatus::Active)).id();
    let parent = world
        .spawn((
            requires_children_bp(true),
            StageCursor { index: 0 },
            agent_state(),
            children(vec![child]),
            ResolveTransition,
        ))
        .id();
    run_gate_children(&mut world);
    assert!(world.get::<WaitingForChildren>(parent).is_some());
    assert!(world.get::<ResolveTransition>(parent).is_none());
    assert_eq!(
        world.get::<AgentState>(parent).unwrap().status,
        AgentStatus::Waiting
    );

    // Child finishes ⇒ the parent resumes and may transition.
    world.get_mut::<AgentState>(child).unwrap().status = AgentStatus::Complete;
    run_gate_children(&mut world);
    assert!(world.get::<WaitingForChildren>(parent).is_none());
    assert!(world.get::<ResolveTransition>(parent).is_some());
    assert_eq!(
        world.get::<AgentState>(parent).unwrap().status,
        AgentStatus::Active
    );
}

#[test]
fn gate_requires_children_does_not_hold_when_not_required_done_or_absent() {
    let mut world = World::new();
    // requires_children = false, even with a running child ⇒ not held.
    let c1 = world.spawn(state_with(AgentStatus::Active)).id();
    let p_norequire = world
        .spawn((
            requires_children_bp(false),
            StageCursor { index: 0 },
            agent_state(),
            children(vec![c1]),
            ResolveTransition,
        ))
        .id();
    // requires_children = true but the child is already terminal ⇒ not held.
    let c2 = world.spawn(state_with(AgentStatus::Complete)).id();
    let p_done = world
        .spawn((
            requires_children_bp(true),
            StageCursor { index: 0 },
            agent_state(),
            children(vec![c2]),
            ResolveTransition,
        ))
        .id();
    // requires_children = true but the child entity no longer exists ⇒ not held.
    let p_ghost = world
        .spawn((
            requires_children_bp(true),
            StageCursor { index: 0 },
            agent_state(),
            children(vec![
                Entity::from_raw_u32(999_999)
                    .expect("a small literal index is always a valid entity id"),
            ]),
            ResolveTransition,
        ))
        .id();
    run_gate_children(&mut world);
    for p in [p_norequire, p_done, p_ghost] {
        assert!(world.get::<ResolveTransition>(p).is_some());
        assert!(world.get::<WaitingForChildren>(p).is_none());
    }
}

#[test]
fn gate_requires_children_resume_waits_on_pending_and_clears_missing() {
    let mut world = World::new();
    // Held with a still-running child ⇒ stays waiting.
    let child = world.spawn(state_with(AgentStatus::Active)).id();
    let stuck = world
        .spawn((agent_state(), children(vec![child]), WaitingForChildren))
        .id();
    // Held with no children component ⇒ resumes (vacuously done).
    let bare = world.spawn((agent_state(), WaitingForChildren)).id();
    // Held with a missing child entity ⇒ resumes.
    let ghost = world
        .spawn((
            agent_state(),
            children(vec![
                Entity::from_raw_u32(999_999)
                    .expect("a small literal index is always a valid entity id"),
            ]),
            WaitingForChildren,
        ))
        .id();
    run_gate_children(&mut world);
    assert!(world.get::<WaitingForChildren>(stuck).is_some());
    assert!(world.get::<ResolveTransition>(stuck).is_none());
    for p in [bare, ghost] {
        assert!(world.get::<WaitingForChildren>(p).is_none());
        assert!(world.get::<ResolveTransition>(p).is_some());
    }
}

fn world_with_compaction_results() -> (World, mpsc::UnboundedSender<CompactionOutcome>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(CompactionResults(rx));
    (world, tx)
}

fn run_collect_compaction(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(collect_compaction);
    s.run(world);
}

#[test]
fn collect_compaction_stores_summary_and_clears_source() {
    let (mut world, tx) = world_with_compaction_results();
    let e = world.spawn((compacting_window(), AwaitingCompaction)).id();
    tx.send(CompactionOutcome {
        entity: e,
        result: Ok(vec![("conv".to_string(), "the summary".to_string())]),
    })
    .unwrap();

    run_collect_compaction(&mut world);

    let w = world.get::<ContextWindow>(e).unwrap();
    assert_eq!(w.get_region("conv").unwrap().current_tokens, 0); // source cleared
    assert!(w.get_region("history").unwrap().current_tokens > 0); // summary stored
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<AwaitingCompaction>(e).is_none());
}

#[test]
fn collect_compaction_error_leaves_context_and_readies() {
    let (mut world, tx) = world_with_compaction_results();
    let e = world.spawn((compacting_window(), AwaitingCompaction)).id();
    let before = world
        .get::<ContextWindow>(e)
        .unwrap()
        .get_region("conv")
        .unwrap()
        .current_tokens;
    tx.send(CompactionOutcome {
        entity: e,
        result: Err(leviath_providers::ProviderError::Other("boom".to_string())),
    })
    .unwrap();

    run_collect_compaction(&mut world);

    // Context untouched on failure, but the agent proceeds.
    assert_eq!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("conv")
            .unwrap()
            .current_tokens,
        before
    );
    assert!(world.get::<ReadyToInfer>(e).is_some());
}

#[test]
fn collect_compaction_drops_stale_outcome() {
    let (mut world, tx) = world_with_compaction_results();
    let ghost = world.spawn_empty().id();
    tx.send(CompactionOutcome {
        entity: ghost,
        result: Ok(vec![]),
    })
    .unwrap();
    run_collect_compaction(&mut world); // no matching agent ⇒ dropped
}

#[test]
fn collect_compaction_summary_for_unpaired_region_is_skipped() {
    // A summary for a region with no paired CompactHistory still clears the
    // source (exercises the None history branch).
    let (mut world, tx) = world_with_compaction_results();
    let mut w = ContextWindow::new(100);
    let mut lone = Region::new(
        "lone".to_string(),
        RegionKind::Compacting {
            threshold_tokens: 5,
        },
        100,
    );
    let _ = lone.add_entry("z".repeat(80), 20);
    w.add_region(lone);
    w.current_tokens = w.calculate_tokens();
    let e = world.spawn((w, AwaitingCompaction)).id();
    tx.send(CompactionOutcome {
        entity: e,
        // "lone" exists but is unpaired (history None); "gone" doesn't exist
        // at all (get_region_mut None) - both no-op branches.
        result: Ok(vec![
            ("lone".to_string(), "s".to_string()),
            ("gone".to_string(), "s2".to_string()),
        ]),
    })
    .unwrap();

    run_collect_compaction(&mut world);

    assert_eq!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("lone")
            .unwrap()
            .current_tokens,
        0
    );
}

// ── persistence dispatch ──

fn run_metadata() -> RunMetadata {
    RunMetadata {
        run_id: "run-1".to_string(),
        agent_name: "a".to_string(),
        agent_path: "/p".to_string(),
        task: "t".to_string(),
        model: None,
        workdir: "/w".to_string(),
        num_stages: 1,
        started_at: 0,
        parent_run_id: None,
        metadata: std::collections::HashMap::new(),
        callback_url: None,
        callback_secret: None,
        title: None,
    }
}

fn world_with_persistence() -> (World, mpsc::UnboundedReceiver<PersistJob>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(PersistenceStage(tx));
    (world, rx)
}

fn run_dispatch_persistence(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(dispatch_persistence);
    s.run(world);
}

// ── interaction-status reflection ──

fn run_reflect(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(reflect_interaction_status);
    s.run(world);
}

fn reflect_state(id: &str, status: AgentStatus) -> AgentState {
    AgentState {
        agent_id: id.to_string(),
        status,
        ..agent_state()
    }
}

/// Register an open request for `agent_id` and wait for it to land in the
/// hub. Returns the join handle for the still-awaiting `ask` so the caller
/// can drop it at the end.
async fn open_request(
    hub: &InteractionHub,
    agent_id: &str,
    request_id: &str,
) -> tokio::task::JoinHandle<leviath_core::interaction::InteractionResponse> {
    use crate::dynamic_interaction::InteractionBackend;
    let backend = hub.backend_for(agent_id.to_string());
    let rid = request_id.to_string();
    let handle = tokio::spawn(async move {
        backend
            .ask(leviath_core::interaction::InteractionRequest::free_text(
                rid, "p", "s", true,
            ))
            .await
    });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    handle
}

#[tokio::test]
async fn reflect_flips_active_to_waiting_and_back_when_prompt_clears() {
    let hub = InteractionHub::new();
    let asking = open_request(&hub, "a", "q1").await;

    let mut world = World::new();
    world.insert_resource(hub.clone());
    let e = world.spawn(reflect_state("a", AgentStatus::Active)).id();

    // Open prompt ⇒ Active → Waiting, tagged AwaitingInteraction.
    run_reflect(&mut world);
    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Waiting
    );
    assert!(world.get::<AwaitingInteraction>(e).is_some());

    // Still pending, already marked ⇒ no-op (the `(true, true)` arm).
    run_reflect(&mut world);
    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Waiting
    );

    // Answered ⇒ Waiting → Active, marker removed.
    assert!(
        hub.answer(leviath_core::interaction::InteractionResponse::text(
            "q1", "ok"
        ))
    );
    run_reflect(&mut world);
    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Active
    );
    assert!(world.get::<AwaitingInteraction>(e).is_none());

    // No pending, no marker ⇒ no-op (the `(false, false)` arm).
    run_reflect(&mut world);
    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Active
    );
    let _ = asking.await;
}

#[tokio::test]
async fn reflect_does_not_flip_a_non_active_agent_with_an_open_prompt() {
    // A terminal agent that happens to still have an open hub entry is left
    // as-is (the inner `status == Active` guard) - no spurious Waiting.
    let hub = InteractionHub::new();
    let asking = open_request(&hub, "a", "q1").await;

    let mut world = World::new();
    world.insert_resource(hub.clone());
    let e = world.spawn(reflect_state("a", AgentStatus::Complete)).id();

    run_reflect(&mut world);
    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Complete
    );
    assert!(world.get::<AwaitingInteraction>(e).is_none());
    hub.cancel("q1");
    let _ = asking.await;
}

#[test]
fn reflect_clears_a_stale_marker_without_reviving_a_terminal_agent() {
    // Marker present, request gone, but the agent has since gone terminal:
    // remove the marker but leave the terminal status untouched (the
    // `status == Waiting` guard on the restore path).
    let hub = InteractionHub::new(); // empty ⇒ nothing pending
    let mut world = World::new();
    world.insert_resource(hub);
    let e = world
        .spawn((
            reflect_state("a", AgentStatus::Cancelled),
            AwaitingInteraction,
        ))
        .id();

    run_reflect(&mut world);
    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Cancelled
    );
    assert!(world.get::<AwaitingInteraction>(e).is_none());
}

#[test]
fn reflect_is_a_noop_without_a_hub_resource() {
    // Test worlds don't install the hub; the system must not panic and must
    // leave agents untouched.
    let mut world = World::new();
    let e = world.spawn(reflect_state("a", AgentStatus::Active)).id();
    run_reflect(&mut world);
    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Active
    );
    assert!(world.get::<AwaitingInteraction>(e).is_none());
}

fn spawn_persistable(world: &mut World) -> Entity {
    world
        .spawn((
            run_metadata(),
            agent_state(),
            conv_window(),
            StageCursor { index: 0 },
            TokenTotals::default(),
            PersistWatermark::default(),
        ))
        .id()
}

#[test]
fn persistence_writes_on_first_dispatch_then_debounces() {
    let (mut world, mut rx) = world_with_persistence();
    let _e = spawn_persistable(&mut world);

    run_dispatch_persistence(&mut world);
    let job = rx.try_recv().expect("first snapshot written");
    assert_eq!(job.run_id, "run-1");

    // No change ⇒ no second write.
    run_dispatch_persistence(&mut world);
    assert!(rx.try_recv().is_err());
}

#[test]
fn persistence_rewrites_when_iteration_changes() {
    let (mut world, mut rx) = world_with_persistence();
    let e = spawn_persistable(&mut world);

    run_dispatch_persistence(&mut world);
    let _ = rx.try_recv().expect("first snapshot");

    world.get_mut::<AgentState>(e).unwrap().iteration += 1;
    run_dispatch_persistence(&mut world);
    let job = rx.try_recv().expect("second snapshot after change");
    assert_eq!(job.meta.iteration, 1);
}

#[test]
fn persistence_rewrites_when_status_changes() {
    let (mut world, mut rx) = world_with_persistence();
    let e = spawn_persistable(&mut world);
    run_dispatch_persistence(&mut world);
    let _ = rx.try_recv().expect("first snapshot");

    world.get_mut::<AgentState>(e).unwrap().status = AgentStatus::Complete;
    run_dispatch_persistence(&mut world);
    let job = rx.try_recv().expect("snapshot after completion");
    assert_eq!(job.meta.status, leviath_core::run_meta::RunStatus::Complete);
}

// ── async LLM-choice transition ──

fn plain_edge(target: &str) -> leviath_core::blueprint::TransitionEdge {
    leviath_core::blueprint::TransitionEdge {
        target: target.to_string(),
        condition: leviath_core::blueprint::TransitionCondition::LlmChoice,
        hint: None,
        transform: leviath_core::blueprint::EdgeTransform::Direct,
        gate: None,
        stuck: None,
    }
}

#[test]
fn match_choice_done_completes_when_allowed() {
    let edges = vec![plain_edge("b")];
    assert_eq!(match_transition_choice("DONE", &edges, true), None);
    // Not allowed to complete ⇒ "done" is just text ⇒ falls back to first edge.
    assert_eq!(
        match_transition_choice("done", &edges, false),
        Some("b".to_string())
    );
}

#[test]
fn match_choice_exact_and_word_and_fallback() {
    let edges = vec![plain_edge("review"), plain_edge("plan")];
    // Exact (case-insensitive).
    assert_eq!(
        match_transition_choice("REVIEW", &edges, false),
        Some("review".to_string())
    );
    // The target appears as a whole word in the (single) decision line.
    assert_eq!(
        match_transition_choice("go to plan now", &edges, false),
        Some("plan".to_string())
    );
    // Whole-word match is case-insensitive.
    let mixed = vec![plain_edge("Deploy")];
    assert_eq!(
        match_transition_choice("please deploy it", &mixed, false),
        Some("Deploy".to_string())
    );
    // No match at all ⇒ first edge (stage cannot complete).
    assert_eq!(
        match_transition_choice("nonsense", &edges, false),
        Some("review".to_string())
    );
    // No edges ⇒ nothing to pick.
    assert_eq!(match_transition_choice("x", &[], false), None);
}

#[test]
fn match_choice_ignores_stage_names_buried_in_prose() {
    // Regression: a review stage's verbose transition response that mentions
    // "the implementation" must NOT be routed back to the `implement` edge -
    // "implementation" is not the whole word "implement". With no clear
    // decision and allow_complete, the run ends (the review approved).
    let edges = vec![plain_edge("implement"), plain_edge("error_recovery")];
    let verbose = "## Review of `test.py`\n\n- The implementation correctly \
                   follows the approved plan. Runs on Python 3.\n\nAPPROVED.";
    assert_eq!(match_transition_choice(verbose, &edges, true), None);
    // Same response in a stage that cannot complete ⇒ first edge, not a
    // prose false-positive.
    assert_eq!(
        match_transition_choice(verbose, &edges, false),
        Some("implement".to_string())
    );
}

#[test]
fn match_choice_reads_done_from_a_verbose_first_line() {
    // "DONE" leading a multi-line summary still completes a completable stage.
    let edges = vec![plain_edge("implement")];
    let resp = "DONE\n\n## Summary\nThe task is complete; no further work needed.";
    assert_eq!(match_transition_choice(resp, &edges, true), None);
    // But a stage that cannot complete ignores the "DONE" and advances along
    // its first edge rather than matching "plan" inside "approved plan".
    let edges2 = vec![plain_edge("review"), plain_edge("plan")];
    let resp2 = "DONE\n\nThe approved plan was implemented; no further work.";
    assert_eq!(
        match_transition_choice(resp2, &edges2, false),
        Some("review".to_string())
    );
}

#[test]
fn match_choice_reads_decision_from_the_concluding_line() {
    // Some models put the answer at the end after reasoning.
    let edges = vec![plain_edge("implement"), plain_edge("error_recovery")];
    let resp = "The tests still fail on the edge case.\n\nimplement";
    assert_eq!(
        match_transition_choice(resp, &edges, true),
        Some("implement".to_string())
    );
}

#[test]
fn build_transition_prompt_default_variants() {
    let mut with_complete = stage_named("s", None, true, None);
    with_complete.transition_prompt = None;
    let edges = vec![{
        let mut e = plain_edge("next");
        e.hint = Some("go next".to_string());
        e
    }];
    let p = build_transition_prompt(&with_complete, &edges);
    assert!(p.contains("Stage 's' is complete"));
    assert!(p.contains("- next: go next")); // hint rendered
    assert!(p.contains("DONE")); // allow_complete branch

    let no_complete = stage_named("s", None, false, None);
    let p2 = build_transition_prompt(&no_complete, &edges);
    assert!(!p2.contains("DONE"));
    assert!(p2.contains("ONLY the stage name"));
}

#[test]
fn build_transition_prompt_custom_variants() {
    let mut custom = stage_named("s", None, true, None);
    custom.transition_prompt = Some("Pick wisely.".to_string());
    let edges = vec![plain_edge("a")];
    let p = build_transition_prompt(&custom, &edges);
    assert!(p.starts_with("Pick wisely."));
    assert!(p.contains("Available transitions:"));
    assert!(p.contains("DONE"));

    custom.allow_complete = false;
    let p2 = build_transition_prompt(&custom, &edges);
    assert!(!p2.contains("DONE"));
    assert!(p2.contains("nothing else"));
}

fn conv_window() -> ContextWindow {
    let mut w = ContextWindow::new(10_000);
    w.add_region(Region::new(
        "conversation".to_string(),
        RegionKind::Clearable,
        10_000,
    ));
    w
}

fn spawn_choosing_agent(
    world: &mut World,
    bp: leviath_core::Blueprint,
    stage_infs: Vec<StageInference>,
    edges: Vec<leviath_core::blueprint::TransitionEdge>,
) -> Entity {
    world
        .spawn((
            AgentBlueprint(bp),
            StageCursor { index: 0 },
            agent_state(),
            StageProgress::default(),
            StageInferences(stage_infs),
            VisitCounts::default(),
            conv_window(),
            stage_infs_head(),
            AwaitingTransitionChoice(edges),
        ))
        .id()
}

// The choosing agent also carries its current `StageInference` (dispatch reads
// provider/model off it).
fn stage_infs_head() -> StageInference {
    StageInference {
        provider_name: "cfg".to_string(),
        model: "m".to_string(),
        tools: vec![],
        tool_filter: None,
    }
}

#[tokio::test]
async fn dispatch_choice_moves_to_awaiting_response_and_injects_prompt() {
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let (ttx, mut trx) = mpsc::unbounded_channel();
    world.resource_mut::<InferenceStage>().transition_outcomes = ttx;

    let bp = blueprint(vec![
        stage_named("a", None, false, None),
        stage_named("b", None, false, None),
    ]);
    let e = spawn_choosing_agent(
        &mut world,
        bp,
        vec![si("m0"), si("m1")],
        vec![plain_edge("b")],
    );

    let mut schedule = Schedule::default();
    schedule.add_systems(dispatch_transition_choice);
    schedule.run(&mut world);

    assert!(world.get::<AwaitingTransitionResponse>(e).is_some());
    assert!(world.get::<AwaitingTransitionChoice>(e).is_none());
    // Prompt injected into the conversation region.
    assert!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region("conversation")
            .unwrap()
            .current_tokens
            > 0
    );
    // The spawned routing job reports back on the transition lane.
    let outcome = trx.recv().await.expect("routing outcome");
    assert_eq!(outcome.entity, e);
}

#[tokio::test]
async fn dispatch_choice_skips_non_active_agent() {
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let bp = blueprint(vec![
        stage_named("a", None, false, None),
        stage_named("b", None, false, None),
    ]);
    let e = spawn_choosing_agent(
        &mut world,
        bp,
        vec![si("m0"), si("m1")],
        vec![plain_edge("b")],
    );
    world.get_mut::<AgentState>(e).unwrap().status = AgentStatus::Cancelled;

    let mut schedule = Schedule::default();
    schedule.add_systems(dispatch_transition_choice);
    schedule.run(&mut world);

    assert!(world.get::<AwaitingTransitionChoice>(e).is_some()); // stayed
    assert!(world.get::<AwaitingTransitionResponse>(e).is_none());
}

#[tokio::test]
async fn dispatch_choice_stays_when_provider_missing() {
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let bp = blueprint(vec![stage_named("a", None, false, None)]);
    let mut infs = vec![si("m0")];
    infs[0].provider_name = "ghost".to_string();
    let e = spawn_choosing_agent(&mut world, bp, infs, vec![plain_edge("a")]);
    // Override the head StageInference to the missing provider too.
    world.entity_mut(e).insert(StageInference {
        provider_name: "ghost".to_string(),
        model: "m".to_string(),
        tools: vec![],
        tool_filter: None,
    });

    let mut schedule = Schedule::default();
    schedule.add_systems(dispatch_transition_choice);
    schedule.run(&mut world);

    assert!(world.get::<AwaitingTransitionChoice>(e).is_some()); // stayed
}

#[tokio::test]
async fn dispatch_choice_stays_when_pool_full() {
    let mut cfg = InferencePoolConfig::new();
    cfg.set_limit("m", 0); // no permits for model "m"
    let (mut world, _rx) = build_world(InferencePools::new(cfg));
    let bp = blueprint(vec![stage_named("a", None, false, None)]);
    let e = spawn_choosing_agent(&mut world, bp, vec![si("m0")], vec![plain_edge("a")]);

    let mut schedule = Schedule::default();
    schedule.add_systems(dispatch_transition_choice);
    schedule.run(&mut world);

    assert!(world.get::<AwaitingTransitionChoice>(e).is_some()); // stayed
}

fn world_with_transition_results() -> (World, mpsc::UnboundedSender<InferenceOutcome>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(TransitionResults(rx));
    (world, tx)
}

fn spawn_responding_agent(
    world: &mut World,
    bp: leviath_core::Blueprint,
    stage_infs: Vec<StageInference>,
    edges: Vec<leviath_core::blueprint::TransitionEdge>,
) -> Entity {
    let n = stage_infs.len();
    world
        .spawn((
            AgentBlueprint(bp),
            StageCursor { index: 0 },
            agent_state(),
            StageProgress::default(),
            StageInferences(stage_infs),
            setups(n),
            VisitCounts::default(),
            conv_window(),
            AwaitingTransitionResponse(edges),
        ))
        .id()
}

fn run_collect_transition(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(collect_transition_choice);
    s.run(world);
}

#[test]
fn collect_choice_enters_chosen_stage() {
    let (mut world, tx) = world_with_transition_results();
    let bp = blueprint(vec![
        stage_named("a", None, false, None),
        stage_named("b", None, false, None),
    ]);
    let e = spawn_responding_agent(
        &mut world,
        bp,
        vec![si("m0"), si("m1")],
        vec![plain_edge("b")],
    );
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Ok(resp("b")),
    })
    .unwrap();

    run_collect_transition(&mut world);

    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    assert_eq!(world.get::<StageInference>(e).unwrap().model, "m1");
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<AwaitingTransitionResponse>(e).is_none());
    assert_eq!(world.get::<AgentState>(e).unwrap().current_stage, "b");
}

/// A transition choice that lands after the run was cancelled is discarded.
/// Notably the no-match arm sets `Complete` unconditionally, which would
/// report a cancelled run as having finished normally.
#[test]
fn collect_choice_does_not_resurrect_or_complete_a_cancelled_run() {
    for choice in ["b", "not-a-stage"] {
        let (mut world, tx) = world_with_transition_results();
        let bp = blueprint(vec![
            stage_named("a", None, false, None),
            stage_named("b", None, false, None),
        ]);
        let e = spawn_responding_agent(
            &mut world,
            bp,
            vec![si("m0"), si("m1")],
            vec![plain_edge("b")],
        );
        world.get_mut::<AgentState>(e).unwrap().status = AgentStatus::Cancelled;
        tx.send(InferenceOutcome {
            latency: std::time::Duration::ZERO,
            entity: e,
            result: Ok(resp(choice)),
        })
        .unwrap();

        run_collect_transition(&mut world);

        assert_eq!(
            world.get::<AgentState>(e).unwrap().status,
            AgentStatus::Cancelled,
            "choice {choice:?} left the run cancelled"
        );
        assert_eq!(
            world.get::<StageCursor>(e).unwrap().index,
            0,
            "and did not advance the stage"
        );
        assert!(world.get::<AwaitingTransitionResponse>(e).is_none());
    }
}

#[test]
fn collect_choice_applies_the_chosen_edge_transform() {
    let (mut world, tx) = world_with_transition_results();
    let bp = blueprint(vec![
        stage_named("a", None, false, None),
        stage_named("b", None, false, None),
    ]);
    let mut edge = plain_edge("b");
    edge.transform = EdgeTransform::Compact { prompt: None };
    let e = spawn_responding_agent(&mut world, bp, vec![si("m0"), si("m1")], vec![edge]);
    world
        .get_mut::<ContextWindow>(e)
        .unwrap()
        .add_to_region("conversation", "summarize me".to_string(), 10)
        .unwrap();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Ok(resp("b")),
    })
    .unwrap();

    run_collect_transition(&mut world);

    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    // The chosen edge's Compact transform queued the conversation region.
    assert_eq!(
        world.get::<PendingEdgeCompact>(e).unwrap().0,
        vec!["conversation".to_string()]
    );
}

#[test]
fn collect_choice_holds_the_stage_when_the_chosen_edge_is_gated() {
    // The LLM-choice path enforces the same gate as the linear path - and
    // must do so before the edge transform reshapes the context it needs.
    let (mut world, tx) = world_with_transition_results();
    let bp = blueprint(vec![
        writing_stage("impl", vec![]),
        stage_named("review", None, false, None),
    ]);
    let mut edge = plain_edge("review");
    edge.transform = EdgeTransform::Compact { prompt: None };
    edge.gate = Some(gate(None, None));
    let e = spawn_responding_agent(&mut world, bp, vec![si("m0"), si("m1")], vec![edge]);
    world
        .entity_mut(e)
        .insert(crate::persistence::RunOutcomeFlags::default());
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Ok(resp("review")),
    })
    .unwrap();

    run_collect_transition(&mut world);

    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 0);
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<AwaitingTransitionResponse>(e).is_none());
    assert_eq!(world.get::<StageProgress>(e).unwrap().gate_reentries, 1);
    // The transform did NOT run.
    assert!(world.get::<PendingEdgeCompact>(e).is_none());
}

#[test]
fn collect_choice_records_a_forced_gate_and_enters_the_stage() {
    let (mut world, tx) = world_with_transition_results();
    let bp = blueprint(vec![
        writing_stage("impl", vec![]),
        stage_named("review", None, false, None),
    ]);
    let mut edge = plain_edge("review");
    edge.gate = Some(gate(None, None));
    let e = spawn_responding_agent(
        &mut world,
        bp.clone(),
        vec![si("m0"), si("m1")],
        vec![edge.clone()],
    );
    world
        .entity_mut(e)
        // Budget already spent.
        .insert(progress_with(0, 0, 3))
        .insert(crate::persistence::RunOutcomeFlags::default());
    // An agent with no flags component still transitions - it just has
    // nowhere to record the forced gate.
    let unflagged = spawn_responding_agent(&mut world, bp, vec![si("m0"), si("m1")], vec![edge]);
    world.entity_mut(unflagged).insert(progress_with(0, 0, 3));
    for entity in [e, unflagged] {
        tx.send(InferenceOutcome {
            latency: std::time::Duration::ZERO,
            entity,
            result: Ok(resp("review")),
        })
        .unwrap();
    }

    run_collect_transition(&mut world);

    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    assert_eq!(world.get::<StageCursor>(unflagged).unwrap().index, 1);
    assert_eq!(
        world
            .get::<crate::persistence::RunOutcomeFlags>(e)
            .unwrap()
            .0
            .gates_forced,
        1
    );
}

#[test]
fn collect_choice_done_completes() {
    let (mut world, tx) = world_with_transition_results();
    let bp = blueprint(vec![stage_named("a", None, true, None)]); // allow_complete
    let e = spawn_responding_agent(&mut world, bp, vec![si("m0")], vec![plain_edge("a")]);
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Ok(resp("DONE")),
    })
    .unwrap();

    run_collect_transition(&mut world);

    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Complete
    );
    assert!(world.get::<ReadyToInfer>(e).is_none());
}

#[test]
fn collect_choice_unknown_target_falls_back_to_first_stage() {
    let (mut world, tx) = world_with_transition_results();
    // Edge target "b" exists as a stage; the LLM names it, so idx resolves. To
    // exercise the position()-unwrap_or(0) fallback we point the edge at a
    // name that survives matching but isn't a stage.
    let bp = blueprint(vec![stage_named("a", None, false, None)]);
    let e = spawn_responding_agent(&mut world, bp, vec![si("m0")], vec![plain_edge("ghost")]);
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Ok(resp("ghost")),
    })
    .unwrap();

    run_collect_transition(&mut world);

    // Matched "ghost" but no such stage ⇒ idx 0 ⇒ re-enters stage "a".
    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 0);
    assert!(world.get::<ReadyToInfer>(e).is_some());
}

#[test]
fn collect_choice_marks_error_on_failure() {
    let (mut world, tx) = world_with_transition_results();
    let bp = blueprint(vec![stage_named("a", None, false, None)]);
    let e = spawn_responding_agent(&mut world, bp, vec![si("m0")], vec![plain_edge("a")]);
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Err(leviath_providers::ProviderError::Other("boom".to_string())),
    })
    .unwrap();

    run_collect_transition(&mut world);

    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Error {
            message: "boom".to_string()
        }
    );
    assert!(world.get::<AwaitingTransitionResponse>(e).is_none());
}

#[test]
fn collect_choice_drops_stale_outcome() {
    let (mut world, tx) = world_with_transition_results();
    let ghost = world.spawn_empty().id();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: ghost,
        result: Ok(resp("x")),
    })
    .unwrap();
    // No matching AwaitingTransitionResponse agent ⇒ silently dropped.
    run_collect_transition(&mut world);
}

// ─── Telemetry activity recording in the collect systems ─────────────────────

#[test]
fn collect_inference_records_activity_with_provider_and_latency() {
    let (mut world, tx) = world_with_results();
    let e = world
        .spawn((
            agent_state(),
            AwaitingInference,
            StageInference {
                provider_name: "anthropic".to_string(),
                model: "m1".to_string(),
                tools: vec![],
                tool_filter: None,
            },
            crate::telemetry::StageActivity::default(),
        ))
        .id();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::from_millis(1500),
        entity: e,
        result: Ok(resp("hi")),
    })
    .unwrap();

    run_collect(&mut world);

    let activity = world.get::<crate::telemetry::StageActivity>(e).unwrap();
    assert_eq!(
        activity.0,
        vec![crate::telemetry::ActivityRecord::Inference {
            provider: "anthropic".to_string(),
            model: "m1".to_string(),
            latency_ms: 1500,
            prompt_tokens: 1,
            completion_tokens: 1,
            cached_tokens: 0,
            success: true,
        }]
    );
}

#[test]
fn collect_inference_records_a_failed_call_without_stage_inference() {
    let (mut world, tx) = world_with_results();
    let e = world
        .spawn((
            agent_state(),
            AwaitingInference,
            crate::telemetry::StageActivity::default(),
        ))
        .id();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::from_millis(20),
        entity: e,
        result: Err(leviath_providers::ProviderError::Other("boom".to_string())),
    })
    .unwrap();

    run_collect(&mut world);

    let activity = world.get::<crate::telemetry::StageActivity>(e).unwrap();
    assert_eq!(
        activity.0,
        vec![crate::telemetry::ActivityRecord::Inference {
            provider: String::new(),
            model: String::new(),
            latency_ms: 20,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            success: false,
        }]
    );
}

#[test]
fn collect_tools_records_one_activity_per_call_with_error_detection() {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolResults(rx));
    let e = world
        .spawn((
            ctx(&[("conversation", 10_000)]),
            crate::components::InferenceResult {
                response: "r".to_string(),
                tool_calls: vec![tc("c1", "read_file"), tc("c2", "write_file")],
                tokens_used: 0,
                timestamp: 0,
            },
            AwaitingTools,
            crate::telemetry::StageActivity::default(),
        ))
        .id();
    tx.send(ToolOutcome {
        elapsed: std::time::Duration::from_millis(40),
        entity: e,
        results: vec![
            ("c1".to_string(), "file body".to_string()),
            ("c2".to_string(), "[error] denied".to_string()),
        ],
    })
    .unwrap();

    run_collect_tools(&mut world);

    let activity = world.get::<crate::telemetry::StageActivity>(e).unwrap();
    assert_eq!(
        activity.0,
        vec![
            crate::telemetry::ActivityRecord::ToolCall {
                tool_name: "read_file".to_string(),
                batch_latency_ms: 40,
                success: true,
            },
            crate::telemetry::ActivityRecord::ToolCall {
                tool_name: "write_file".to_string(),
                batch_latency_ms: 40,
                success: false,
            },
        ]
    );
}

#[test]
fn collect_compaction_records_success_and_failure() {
    let (mut world, tx) = world_with_compaction_results();
    let e = world
        .spawn((
            compacting_window(),
            AwaitingCompaction,
            crate::telemetry::StageActivity::default(),
        ))
        .id();
    tx.send(CompactionOutcome {
        entity: e,
        result: Ok(vec![("conv".to_string(), "summary".to_string())]),
    })
    .unwrap();
    run_collect_compaction(&mut world);

    let e2 = world
        .spawn((
            compacting_window(),
            AwaitingCompaction,
            crate::telemetry::StageActivity::default(),
        ))
        .id();
    tx.send(CompactionOutcome {
        entity: e2,
        result: Err(leviath_providers::ProviderError::Other("boom".to_string())),
    })
    .unwrap();
    run_collect_compaction(&mut world);

    assert_eq!(
        world.get::<crate::telemetry::StageActivity>(e).unwrap().0,
        vec![crate::telemetry::ActivityRecord::Compaction { success: true }]
    );
    assert_eq!(
        world.get::<crate::telemetry::StageActivity>(e2).unwrap().0,
        vec![crate::telemetry::ActivityRecord::Compaction { success: false }]
    );
}
