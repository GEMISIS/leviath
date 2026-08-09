//! The former single-file test module, exercising every pipeline section.
//! `use super::*;` sees the whole pipeline surface through mod.rs's
//! re-exports, exactly as it did when the sections were inline.

use super::*;
use crate::inference_pool::{InferencePoolConfig, InferencePools};
use crate::test_support::hints;
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
        _r: &InferenceRequest,
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
        fallbacks: Vec::new(),
        output: None,
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
        shell_hint: false,
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
        shell_hint: false,
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
        shell_hint: false,
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
        shell_hint: false,
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

/// An [`InferenceConfig`] with just the two hint toggles set, everything else
/// at its inert default.
fn hint_config(batch_tool_hint: bool, shell_hint: bool) -> InferenceConfig {
    InferenceConfig {
        temperature: None,
        max_output_tokens: None,
        extra_params: Default::default(),
        batch_tool_hint,
        shell_hint,
        request_timeout_secs: None,
    }
}

#[test]
fn shell_guidance_is_windows_only() {
    // The one platform whose shell isn't what a model assumes.
    assert_eq!(shell_guidance_for("windows"), Some(WINDOWS_SHELL_HINT));
    assert!(WINDOWS_SHELL_HINT.contains("cmd.exe"));
    // Everywhere else a POSIX shell is the default assumption, so nothing to
    // say - including for an OS string this build has never heard of.
    assert_eq!(shell_guidance_for("linux"), None);
    assert_eq!(shell_guidance_for("macos"), None);
    assert_eq!(shell_guidance_for("freebsd"), None);
    assert_eq!(shell_guidance_for("haiku"), None);
}

#[test]
fn the_shell_hint_needs_the_toggle_the_platform_and_the_tool() {
    let shell = vec![tool("shell")];
    let cases = [
        // (shell_hint, os, tools, expected)
        (true, "windows", &shell, true),
        // Opted out at some level of the cascade.
        (false, "windows", &shell, false),
        // A platform whose shell needs no explanation.
        (true, "linux", &shell, false),
        // A stage that cannot run commands doesn't pay for the hint.
        (true, "windows", &vec![tool("read_file")], false),
        (true, "windows", &vec![], false),
    ];
    for (shell_hint, os, tools, expected) in cases {
        let cfg = hint_config(false, shell_hint);
        let blocks = hint_blocks(Some(&cfg), tools, os);
        assert_eq!(
            blocks.iter().any(|b| b.text == WINDOWS_SHELL_HINT),
            expected,
            "shell_hint={shell_hint} os={os} tools={:?}",
            tools.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
    }
    // No config at all is the same as both toggles off.
    assert!(hint_blocks(None, &shell, "windows").is_empty());
}

#[test]
fn both_hints_lead_the_prefix_with_the_batch_hint_first() {
    // Order matters: these are the stable head of the `Always` cache prefix, so
    // it has to be the same head on every request the host makes.
    let cfg = hint_config(true, true);
    let blocks = hint_blocks(Some(&cfg), &[tool("shell")], "windows");
    let texts: Vec<&str> = blocks.iter().map(|b| b.text.as_str()).collect();
    assert_eq!(texts, vec![BATCH_TOOL_HINT, WINDOWS_SHELL_HINT]);
    assert!(
        blocks
            .iter()
            .all(|b| b.cache_hint == leviath_core::CacheHint::Always)
    );
}

#[test]
fn build_request_puts_the_hints_ahead_of_the_stage_context() {
    // `build_request` reads the *host* OS, so the shell hint's presence is not
    // assertable portably here; what is assertable is that whatever hints apply
    // come first and the stage's own blocks survive behind them.
    let cfg = hint_config(true, true);
    let si = stage("m", vec![tool("shell")], None);
    let req = build_request(
        &window_with_sys(),
        Some(&cfg),
        &si,
        &provider(true, 500),
        "test-stage",
        0,
    );
    let hints = hint_blocks(Some(&cfg), &si.tools, std::env::consts::OS);
    for (i, hint) in hints.iter().enumerate() {
        assert_eq!(req.system[i].text, hint.text);
    }
    assert!(
        req.system[hints.len()..]
            .iter()
            .any(|b| b.text.contains("base system")),
        "the stage's own system block is preserved after the hints"
    );
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
async fn dispatch_parks_an_agent_whose_provider_circuit_is_open() {
    // Reaching dispatch on a tripped provider means rotation found nowhere
    // else to go, so sending the request would just burn another failure.
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let policy = CircuitPolicy {
        failures_before_open: 1,
        cooldown_secs: 300,
    };
    let mut circuits = ProviderCircuits::default();
    circuits.record_failure(
        "cfg",
        leviath_providers::UnavailableReason::CreditsExhausted,
        chrono::Utc::now().timestamp(),
        &policy,
    );
    world.insert_resource(circuits);
    world.insert_resource(policy);
    let e = world
        .spawn((
            agent_state(),
            window(),
            stage("m", vec![], None),
            ReadyToInfer,
        ))
        .id();

    run(&mut world);

    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<AwaitingInference>(e).is_none());
    assert_eq!(
        world.get::<DispatchStall>(e).map(|s| s.reason),
        Some(StallReason::ProviderCircuitOpen),
        "the park reason is what `lev ps` and the watchdog read"
    );
}

#[tokio::test]
async fn dispatch_proceeds_once_the_cooldown_lets_a_probe_through() {
    // The probe is what closes the circuit again, so it must reach the wire.
    let (mut world, mut rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    let policy = CircuitPolicy {
        failures_before_open: 1,
        cooldown_secs: 60,
    };
    let mut circuits = ProviderCircuits::default();
    circuits.record_failure(
        "cfg",
        leviath_providers::UnavailableReason::CreditsExhausted,
        chrono::Utc::now().timestamp() - 61,
        &policy,
    );
    world.insert_resource(circuits);
    world.insert_resource(policy);
    let e = world
        .spawn((
            agent_state(),
            window(),
            stage("m", vec![], None),
            ReadyToInfer,
        ))
        .id();

    run(&mut world);

    assert!(world.get::<AwaitingInference>(e).is_some());
    assert!(rx.recv().await.expect("outcome").result.is_ok());
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

/// A provider whose `infer` panics, standing in for any bug that kills a lane
/// task before it can report - the case that used to leave the agent waiting on
/// an outcome that would never arrive (issue #190).
struct Exploding;
#[async_trait::async_trait]
impl Provider for Exploding {
    async fn infer(
        &self,
        _r: &InferenceRequest,
    ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
        panic!("provider adapter blew up")
    }
    async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
        1
    }
    fn max_context_tokens(&self, _m: &str) -> usize {
        100_000
    }
    fn name(&self) -> &str {
        "exploding"
    }
    fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
        leviath_providers::ModelCapabilities::default()
    }
}

/// Register [`Exploding`] under `"exploding"` in an already-built test world.
fn register_exploding(world: &mut World) {
    world
        .resource_mut::<Providers>()
        .0
        .register("exploding".to_string(), Arc::new(Exploding));
}

#[tokio::test]
async fn exploding_provider_metadata_is_exercised() {
    // Keep the mock's non-`infer` trait methods measured.
    let p = Exploding;
    assert_eq!(p.name(), "exploding");
    assert_eq!(p.count_tokens("t", "m").await, 1);
    assert_eq!(p.max_context_tokens("m"), 100_000);
    let _ = p.capabilities("m");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_inference_job_reports_an_error_instead_of_vanishing() {
    let (mut world, mut rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    register_exploding(&mut world);
    let e = world
        .spawn((
            agent_state(),
            window(),
            stage("m", vec![], None).clone_with_provider("exploding"),
            ReadyToInfer,
        ))
        .id();

    let _silent = crate::test_support::SilentPanics::install();
    run(&mut world);

    // The agent is parked on `AwaitingInference`, which the driver reads as
    // "busy" - so an outcome has to arrive or it waits for ever.
    assert!(world.get::<AwaitingInference>(e).is_some());
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("the supervisor reports promptly")
        .expect("an outcome");
    assert_eq!(outcome.entity, e);
    let err = outcome
        .result
        .expect_err("a dead job is an error")
        .to_string();
    assert!(err.contains("panicked"), "got: {err}");
    assert!(err.contains("provider adapter blew up"), "got: {err}");
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

// ── provider failover on an unusable provider (issue #201) ──

/// A stage on `dead/model-a` with one place left to go.
fn stage_with_fallback() -> StageInference {
    StageInference {
        provider_name: "dead".to_string(),
        model: "model-a".to_string(),
        tools: Vec::new(),
        tool_filter: None,
        fallbacks: vec![leviath_core::blueprint::ModelEntry::new(
            "alive".to_string(),
            "model-b".to_string(),
        )],
        output: None,
    }
}

fn credits_exhausted() -> leviath_providers::ProviderError {
    leviath_providers::ProviderError::Unavailable {
        reason: leviath_providers::UnavailableReason::CreditsExhausted,
        detail: "HTTP 402 Payment Required".to_string(),
    }
}

#[test]
fn an_unusable_provider_fails_over_instead_of_killing_the_run() {
    let (mut world, tx) = world_with_results();
    let e = world
        .spawn((agent_state(), AwaitingInference, stage_with_fallback()))
        .id();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Err(credits_exhausted()),
    })
    .unwrap();

    run_collect(&mut world);

    // The stage now points at the fallback and is ready to be dispatched
    // again; the run is still alive.
    let si = world.get::<StageInference>(e).unwrap();
    assert_eq!(si.provider_name, "alive");
    assert_eq!(si.model, "model-b");
    assert!(si.fallbacks.is_empty(), "the candidate was consumed");
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(world.get::<AwaitingInference>(e).is_none());
    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Active
    );
    assert!(world.get::<StageOutcome>(e).is_none());
    assert!(world.get::<ResolveTransition>(e).is_none());
    // The agent never got a turn, so the iteration must not move.
    assert_eq!(world.get::<AgentState>(e).unwrap().iteration, 0);
}

#[test]
fn failover_is_recorded_in_the_stage_log() {
    // A silent swap is how a factory ends up on a model nobody chose.
    let (mut world, tx) = world_with_results();
    let e = world
        .spawn((
            agent_state(),
            AwaitingInference,
            stage_with_fallback(),
            StageIoBuffer::default(),
        ))
        .id();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Err(credits_exhausted()),
    })
    .unwrap();

    run_collect(&mut world);

    let logs = &world.get::<StageIoBuffer>(e).unwrap().logs;
    let line = logs
        .iter()
        .map(|(_, l)| l.as_str())
        .find(|l| l.starts_with("[failover]"))
        .expect("the swap is written to the stage log");
    assert!(line.contains("dead/model-a"), "{line}");
    assert!(line.contains("alive/model-b"), "{line}");
}

#[test]
fn an_exhausted_fallback_list_still_terminates() {
    // Last provider standing: the run ends, but with the readable message
    // rather than the raw JSON body the issue reported.
    let (mut world, tx) = world_with_results();
    let mut si = stage_with_fallback();
    si.fallbacks.clear();
    let e = world.spawn((agent_state(), AwaitingInference, si)).id();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Err(credits_exhausted()),
    })
    .unwrap();

    run_collect(&mut world);

    assert!(world.get::<ReadyToInfer>(e).is_none());
    assert!(world.get::<ResolveTransition>(e).is_some());
    let AgentStatus::Error { message } = &world.get::<AgentState>(e).unwrap().status else {
        panic!("an exhausted chain is a terminal error");
    };
    assert!(message.starts_with("out of credits:"), "{message}");
}

#[test]
fn an_ordinary_error_does_not_burn_a_fallback() {
    // Failing over on a malformed request would waste the one provider that
    // still works, so only a provider-fatal error may consume a candidate.
    let (mut world, tx) = world_with_results();
    let e = world
        .spawn((agent_state(), AwaitingInference, stage_with_fallback()))
        .id();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Err(leviath_providers::ProviderError::ApiError(
            "HTTP 400: bad request".to_string(),
        )),
    })
    .unwrap();

    run_collect(&mut world);

    let si = world.get::<StageInference>(e).unwrap();
    assert_eq!(si.provider_name, "dead", "the provider is untouched");
    assert_eq!(si.fallbacks.len(), 1, "the candidate is still available");
    assert!(world.get::<ResolveTransition>(e).is_some());
}

#[test]
fn provider_fatal_failures_trip_the_breaker_and_a_success_clears_it() {
    // Failing over rescues *this* run. The breaker is what stops the next ten
    // runs each rediscovering the same dead account (issue #201).
    let (mut world, tx) = world_with_results();
    let policy = CircuitPolicy {
        failures_before_open: 2,
        cooldown_secs: 300,
    };
    world.insert_resource(ProviderCircuits::default());
    world.insert_resource(policy);
    let now = chrono::Utc::now().timestamp();

    for _ in 0..2 {
        let e = world
            .spawn((agent_state(), AwaitingInference, stage_with_fallback()))
            .id();
        tx.send(InferenceOutcome {
            latency: std::time::Duration::ZERO,
            entity: e,
            result: Err(credits_exhausted()),
        })
        .unwrap();
        run_collect(&mut world);
    }
    assert!(
        world
            .resource::<ProviderCircuits>()
            .is_open("dead", now, &policy),
        "two strikes at a threshold of two opens the circuit"
    );

    // A later success on that provider puts it straight back into service.
    let e = world
        .spawn((agent_state(), AwaitingInference, stage_with_fallback()))
        .id();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Ok(resp("hi")),
    })
    .unwrap();
    run_collect(&mut world);
    assert!(
        !world
            .resource::<ProviderCircuits>()
            .is_open("dead", now, &policy)
    );
}

#[test]
fn an_ordinary_error_does_not_count_against_the_provider() {
    // A malformed request is our fault, not the provider's. Counting it would
    // take a perfectly healthy provider out of service.
    let (mut world, tx) = world_with_results();
    let policy = CircuitPolicy {
        failures_before_open: 1,
        cooldown_secs: 300,
    };
    world.insert_resource(ProviderCircuits::default());
    world.insert_resource(policy);
    let e = world
        .spawn((agent_state(), AwaitingInference, stage_with_fallback()))
        .id();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Err(leviath_providers::ProviderError::ApiError(
            "HTTP 400: bad request".to_string(),
        )),
    })
    .unwrap();

    run_collect(&mut world);

    assert!(!world.resource::<ProviderCircuits>().is_open(
        "dead",
        chrono::Utc::now().timestamp(),
        &policy
    ));
}

#[test]
fn collect_works_without_the_breaker_installed() {
    // The resources are optional, so an embedder that never inserts them keeps
    // the plain failover behavior.
    let (mut world, tx) = world_with_results();
    let e = world
        .spawn((agent_state(), AwaitingInference, stage_with_fallback()))
        .id();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Err(credits_exhausted()),
    })
    .unwrap();

    run_collect(&mut world);

    assert_eq!(
        world.get::<StageInference>(e).unwrap().provider_name,
        "alive"
    );
}

#[test]
fn an_unusable_provider_without_a_stage_component_still_terminates() {
    // `StageInference` is optional on the query, so the failover branch has to
    // cope with its absence rather than assuming one is attached.
    let (mut world, tx) = world_with_results();
    let e = world.spawn((agent_state(), AwaitingInference)).id();
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Err(credits_exhausted()),
    })
    .unwrap();

    run_collect(&mut world);

    assert!(world.get::<ReadyToInfer>(e).is_none());
    assert!(world.get::<ResolveTransition>(e).is_some());
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

    let job = snapshot_job(rx.try_recv().expect("job sent"));
    assert_eq!(job.stages.len(), 2);
    assert_eq!(job.stages[0].name, "plan");
    assert_eq!(job.stages[0].status, StageRunStatus::Active);
    assert_eq!(job.output_appends, vec![(0, "hello".to_string())]);
    assert_eq!(job.log_appends, vec![(0, "[tool] x: y".to_string())]);
    // The buffer was drained in place.
    assert!(world.get::<StageIoBuffer>(e).unwrap().output.is_empty());
}

/// Every snapshot carries the answer's bytes whenever the agent holds them.
///
/// It used to send them once and rely on a sender-side watermark thereafter.
/// That watermark advanced when the job was *built*, but the persistence lane
/// coalesces queued snapshots per run and keeps only the newest - so a run that
/// finished inside one persistence window had the job carrying the body dropped
/// as superseded, while every later job still wrote `meta.json`'s descriptor.
/// The two halves then disagreed for good, and `read_final_output` reads that as
/// "no answer" (issue #276).
///
/// Not writing the same quarter-megabyte file on every heartbeat is still worth
/// doing; it now happens in the lane, past the coalescing, where whether a job
/// was written is a fact rather than an assumption.
#[test]
fn dispatch_persistence_always_carries_the_answer_for_the_lane_to_judge() {
    let (mut world, mut rx) = world_with_persistence();
    let e = world
        .spawn((
            run_metadata(),
            agent_state(),
            conv_window(),
            StageCursor { index: 0 },
            TokenTotals::default(),
            PersistWatermark::default(),
            crate::persistence::FinalOutput(leviath_core::output::FinalOutput::new(
                "the first answer",
                None,
                "summary".to_string(),
                100,
            )),
        ))
        .id();

    run_dispatch_persistence(&mut world);
    let first = snapshot_job(rx.try_recv().expect("job sent"));
    assert_eq!(first.final_output.as_deref(), Some("the first answer"));
    // The descriptor rides in meta either way.
    assert_eq!(
        first.meta.final_output.as_ref().map(|d| d.bytes),
        Some("the first answer".len())
    );

    // A second tick carries the same answer again. Backdating the watermark
    // makes the heartbeat due, which is the tick that previously sent nothing.
    //
    // This is the assertion that pins the bug: if this snapshot were the one to
    // survive coalescing and it carried no body, the descriptor below would
    // reach `meta.json` with no sidecar beside it.
    world
        .get_mut::<PersistWatermark>(e)
        .expect("watermark present")
        .backdate(0);
    run_dispatch_persistence(&mut world);
    let second = snapshot_job(rx.try_recv().expect("job sent"));
    assert_eq!(
        second.final_output.as_deref(),
        Some("the first answer"),
        "a snapshot that describes an answer must also carry it"
    );
    assert!(second.meta.final_output.is_some(), "and still describes it");
    // The pairing itself, stated once: describing without carrying is exactly
    // the state that leaves `lev result` reporting no output.
    assert_eq!(
        second.meta.final_output.is_some(),
        second.final_output.is_some(),
        "descriptor and body must travel together"
    );

    // A new submission is written again.
    world.entity_mut(e).insert(crate::persistence::FinalOutput(
        leviath_core::output::FinalOutput::new(
            "the corrected answer",
            None,
            "summary".to_string(),
            200,
        ),
    ));
    world
        .get_mut::<PersistWatermark>(e)
        .expect("watermark present")
        .backdate(0);
    run_dispatch_persistence(&mut world);
    let third = snapshot_job(rx.try_recv().expect("job sent"));
    assert_eq!(third.final_output.as_deref(), Some("the corrected answer"));
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

    let job = snapshot_job(rx.try_recv().expect("job sent"));
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
                results_region: None,
                max_items: None,
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
    let job = snapshot_job(rx.try_recv().expect("job sent"));
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
    let job = snapshot_job(rx.try_recv().expect("job sent"));
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
    let job = snapshot_job(rx.try_recv().expect("job sent"));
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
    assert!(
        snapshot_job(rx.try_recv().expect("job sent"))
            .interactions
            .is_none()
    );
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
    assert!(
        snapshot_job(rx.try_recv().expect("job sent"))
            .interactions
            .is_none()
    );
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

    // Watermark unchanged and no heartbeat due, but new buffered content ⇒
    // the lines are journaled WITHOUT a whole-window snapshot. Snapshotting
    // per log-line batch deep-cloned the context several times per iteration.
    world
        .get_mut::<StageIoBuffer>(e)
        .unwrap()
        .logs
        .push((0, "late log".to_string()));
    run_dispatch_persistence(&mut world);
    match rx.try_recv().expect("append-triggered message") {
        PersistMsg::StageLines {
            run_id,
            output_appends,
            log_appends,
        } => {
            assert_eq!(run_id, "run-1");
            assert!(output_appends.is_empty());
            assert_eq!(log_appends, vec![(0, "late log".to_string())]);
        }
        PersistMsg::Snapshot(_) | PersistMsg::Append { .. } => {
            panic!("buffered lines alone must not force a whole-window snapshot")
        }
    }
    // The buffer was drained in place either way.
    assert!(world.get::<StageIoBuffer>(e).unwrap().logs.is_empty());
}

/// Buffered lines when the heartbeat IS due ride the full snapshot rather
/// than a lines-only message, so `updated_at` still advances.
#[test]
fn dispatch_persistence_appends_ride_the_snapshot_when_heartbeat_is_due() {
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

    run_dispatch_persistence(&mut world);
    let _ = rx.try_recv().expect("first job");

    // Age the watermark past the heartbeat window, then buffer a line.
    let stale = chrono::Utc::now().timestamp() - (PERSIST_HEARTBEAT_SECS + 1);
    world
        .get_mut::<PersistWatermark>(e)
        .unwrap()
        .backdate(stale);
    world
        .get_mut::<StageIoBuffer>(e)
        .unwrap()
        .logs
        .push((0, "heartbeat log".to_string()));
    run_dispatch_persistence(&mut world);
    let job = snapshot_job(rx.try_recv().expect("heartbeat job"));
    assert_eq!(job.log_appends, vec![(0, "heartbeat log".to_string())]);
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

/// Broadcast log lines are truncated (the never-shrinking ring retains every
/// slot's strings); the on-disk stage log keeps the full line.
#[test]
fn dispatch_persistence_truncates_long_broadcast_lines_but_not_disk_appends() {
    use crate::host::{WorldEvent, WorldEventSink};
    let (mut world, mut rx) = world_with_persistence();
    let (sink_tx, mut sink_rx) = tokio::sync::broadcast::channel(16);
    world.insert_resource(WorldEventSink(sink_tx));
    let long_line = "y".repeat(BROADCAST_LOG_LINE_MAX_BYTES + 100);
    let mut buf = StageIoBuffer::default();
    buf.output.push((0, long_line.clone()));
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

    let event = sink_rx.try_recv().expect("log event");
    let WorldEvent::Log { line, .. } = event else {
        panic!("expected a Log event");
    };
    assert!(line.len() < long_line.len(), "broadcast copy is truncated");
    assert!(line.ends_with("[truncated 100 bytes]"), "got: {line}");
    // The disk append still carries the whole line.
    let job = snapshot_job(rx.try_recv().expect("persist job"));
    assert_eq!(job.output_appends, vec![(0, long_line)]);
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
    world.insert_resource(ToolStage::detached(jtx));
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

    let job = snapshot_job(prx.try_recv().expect("persist job"));
    let (idx, json) = job.taint_audit.expect("taint audit persisted");
    assert_eq!(idx, 1);
    assert!(json.contains("shell"));
}

/// An unchanged audit log is not re-serialized on the next snapshot: the file
/// on disk is already current, and rewriting it every heartbeat was an
/// O(events) allocation that grew with the run.
#[test]
fn dispatch_persistence_taint_audit_is_not_rewritten_when_unchanged() {
    let (mut world, mut prx) = world_with_persistence();
    let (jtx, _jrx) = mpsc::unbounded_channel();
    world.insert_resource(ToolServiceRes(std::sync::Arc::new(EchoService)));
    world.insert_resource(ToolStage::detached(jtx));
    let e = world
        .spawn((
            run_metadata(),
            agent_state(),
            infer_with(vec![tc("c_shell", "shell")]),
            tainted_conv_window(),
            ReadyForTools,
            enabled_gate(),
            StageCursor { index: 1 },
            TokenTotals::default(),
            PersistWatermark::default(),
        ))
        .id();
    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);
    run_dispatch_persistence(&mut world);
    let first = snapshot_job(prx.try_recv().expect("first job"));
    assert!(first.taint_audit.is_some(), "first write carries the audit");

    // Force a heartbeat snapshot with no new gate events: the audit rides
    // along exactly once.
    let stale = chrono::Utc::now().timestamp() - (PERSIST_HEARTBEAT_SECS + 1);
    world
        .get_mut::<PersistWatermark>(e)
        .unwrap()
        .backdate(stale);
    run_dispatch_persistence(&mut world);
    let second = snapshot_job(prx.try_recv().expect("heartbeat job"));
    assert!(
        second.taint_audit.is_none(),
        "an unchanged audit log is not re-serialized"
    );
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
    let job = snapshot_job(prx.try_recv().expect("persist job"));
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
        hints(true),
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
        hints(true),
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
            hints(true),
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
        hints(true),
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
        hints(true),
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
        hints(true),
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
        hints(true),
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
            unattended: leviath_core::blueprint::UnattendedPolicy::AutoApprove,
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
        text_only_nudges: leviath_core::blueprint::DEFAULT_MAX_NUDGES,
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
    // The default text goes in through the shared `[System]` injection path.
    let injected = conversation_text(&world, e);
    assert!(injected.contains(&format!(
        "[System] {}",
        leviath_core::blueprint::DEFAULT_NUDGE_TEXT
    )));
}

#[test]
fn empty_response_respects_a_stage_that_disables_its_nudge() {
    // The issue-#127 shape: the stage knows its deliverable is text and says so.
    let mut bp = nudge_bp(false);
    bp.0.stages[0].nudge = Some(leviath_core::NudgeConfig {
        enabled: Some(false),
        ..Default::default()
    });
    let mut world = World::new();
    let e = world
        .spawn((
            ctx(&[("conversation", 10_000)]),
            infer_result(false),
            StageProgress::default(),
            bp,
            StageCursor { index: 0 },
            ReadyForTransition,
        ))
        .id();
    run_empty(&mut world);
    assert!(world.get::<ResolveTransition>(e).is_some());
    assert_eq!(world.get::<StageProgress>(e).unwrap().text_only_nudges, 0);
    assert!(conversation_text(&world, e).is_empty());
}

#[test]
fn empty_response_honors_an_agent_level_max() {
    // `[agent.nudge] max = 0`: the very first text-only response is final.
    let mut bp = nudge_bp(false);
    bp.0.nudge = Some(leviath_core::NudgeConfig {
        max: Some(0),
        ..Default::default()
    });
    let mut world = World::new();
    let e = world
        .spawn((
            ctx(&[("conversation", 10_000)]),
            infer_result(false),
            StageProgress::default(),
            bp,
            StageCursor { index: 0 },
            ReadyForTransition,
        ))
        .id();
    run_empty(&mut world);
    assert!(world.get::<ResolveTransition>(e).is_some());
    assert!(world.get::<ReadyToInfer>(e).is_none());
}

#[test]
fn empty_response_interpolates_custom_text_placeholders() {
    // A custom text names the stage and its required regions.
    let mut bp = nudge_bp(false);
    bp.0.stages[0].nudge = Some(leviath_core::NudgeConfig {
        text: Some("Populate {regions} to finish stage {stage}.".to_string()),
        ..Default::default()
    });
    bp.0.context_layout
        .regions
        .push(leviath_core::layout::RegionDefinition::new(
            "plan".to_string(),
            RegionKind::Pinned,
            1_000,
        ));
    bp.0.context_layout.regions[1].required = true;
    let mut world = World::new();
    let e = world
        .spawn((
            ctx(&[("conversation", 10_000)]),
            infer_result(false),
            StageProgress::default(),
            bp,
            StageCursor { index: 0 },
            ReadyForTransition,
        ))
        .id();
    run_empty(&mut world);
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(
        conversation_text(&world, e).contains("[System] Populate plan to finish stage a."),
        "placeholders resolve against the stage name and required region names"
    );
}

#[test]
fn empty_response_explicit_enabled_overrides_review_suppression() {
    // The inverse of `empty_response_never_nudges_a_stage_whose_output_is
    // _reviewed`: the suppression is only the default, and a stage author who
    // explicitly asks for nudging on a reviewed stage gets it.
    let mut bp = nudge_bp(true);
    bp.0.stages[0].nudge = Some(leviath_core::NudgeConfig {
        enabled: Some(true),
        ..Default::default()
    });
    let mut world = World::new();
    let e = world
        .spawn((
            ctx(&[("conversation", 10_000)]),
            infer_result(false),
            StageProgress::default(),
            bp,
            StageCursor { index: 0 },
            ReadyForTransition,
        ))
        .id();
    run_empty(&mut world);
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert_eq!(world.get::<StageProgress>(e).unwrap().text_only_nudges, 1);
}

#[test]
fn empty_response_reads_the_global_nudge_component() {
    // A spawn-time `GlobalNudge` snapshot participates in the cascade when the
    // blueprint sets nothing at either level.
    let mut world = World::new();
    let e = world
        .spawn((
            ctx(&[("conversation", 10_000)]),
            infer_result(false),
            StageProgress::default(),
            nudge_bp(false),
            StageCursor { index: 0 },
            ReadyForTransition,
            GlobalNudge(leviath_core::NudgeConfig {
                enabled: Some(false),
                ..Default::default()
            }),
        ))
        .id();
    run_empty(&mut world);
    assert!(world.get::<ResolveTransition>(e).is_some());
    assert!(conversation_text(&world, e).is_empty());
}

#[test]
fn empty_response_with_an_out_of_range_cursor_uses_blueprint_defaults() {
    // A cursor past the stage list (nothing configures the nudge, no stage to
    // name): the default text still goes in and the agent loops back.
    let mut world = World::new();
    let e = world
        .spawn((
            ctx(&[("conversation", 10_000)]),
            infer_result(false),
            StageProgress::default(),
            nudge_bp(false),
            StageCursor { index: 7 },
            ReadyForTransition,
        ))
        .id();
    run_empty(&mut world);
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(conversation_text(&world, e).contains(leviath_core::blueprint::DEFAULT_NUDGE_TEXT));
}

// ── tool-dispatch ──

/// A tool service that echoes each call as `(id, "ran <name>")`.
struct EchoService;
impl ToolService for EchoService {
    fn exec_for(
        &self,
        _entity: Entity,
        calls: Vec<leviath_providers::ToolCall>,
        _progress: ToolProgress,
    ) -> BoxedToolExec {
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
    fn exec_for(
        &self,
        _entity: Entity,
        _calls: Vec<leviath_providers::ToolCall>,
        _progress: ToolProgress,
    ) -> BoxedToolExec {
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
    assert!(
        service.exec_for(entity, Vec::new(), noop_progress())()
            .await
            .is_empty()
    );
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
            Vec::new(),
            noop_progress(),
        )()
        .await
        .is_empty()
    );
}

/// A service whose `refresh_tools` returns a fixed set of tool names.
struct RefreshService(Vec<&'static str>);
impl ToolService for RefreshService {
    fn exec_for(
        &self,
        _e: Entity,
        _c: Vec<leviath_providers::ToolCall>,
        _progress: ToolProgress,
    ) -> BoxedToolExec {
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
        fallbacks: Vec::new(),
        output: None,
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
    fn exec_for(
        &self,
        _e: Entity,
        _c: Vec<leviath_providers::ToolCall>,
        _progress: ToolProgress,
    ) -> BoxedToolExec {
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
            Vec::new(),
            noop_progress(),
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
    world.insert_resource(ToolStage::detached(jtx));
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

// ── batch journaling at dispatch (#96) ──

/// A tool service whose executor reports each call through `progress` before
/// returning - the shape the CLI executor has.
struct ReportingService;
impl ToolService for ReportingService {
    fn exec_for(
        &self,
        _entity: Entity,
        calls: Vec<leviath_providers::ToolCall>,
        progress: ToolProgress,
    ) -> BoxedToolExec {
        Box::new(move || {
            Box::pin(async move {
                calls
                    .into_iter()
                    .map(|c| {
                        let r = format!("ran {}", c.name);
                        progress(&c.id, &r);
                        (c.id, r)
                    })
                    .collect()
            })
        })
    }
}

/// Unwrap the Append message a journaling test expects on the persistence lane.
fn append_msg(
    msg: PersistMsg,
) -> (
    String,
    leviath_core::run_archive::RunRecord,
    Option<tokio::sync::oneshot::Sender<()>>,
) {
    match msg {
        PersistMsg::Append {
            run_id,
            record,
            ack,
        } => (run_id, *record, ack),
        PersistMsg::Snapshot(_) | PersistMsg::StageLines { .. } => {
            panic!("expected an append on the lane")
        }
    }
}

#[tokio::test]
async fn dispatch_journals_the_batch_then_each_completion() {
    use leviath_core::run_archive::RunRecord;
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let (ptx, mut prx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(ReportingService)));
    world.insert_resource(ToolStage::detached(jtx));
    world.insert_resource(PersistenceStage(ptx));
    // A batch mixing an inline-resolved call (a context tool) and a lane call.
    let e = world
        .spawn((
            agent_state(),
            infer_with(vec![
                ctx_call("c_ctx", "notes", "hi"),
                tc("c_lane", "read_file"),
            ]),
            notes_window(),
            StageCursor { index: 0 },
            run_metadata(),
            ReadyForTools,
        ))
        .id();
    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);
    assert!(world.get::<AwaitingTools>(e).is_some());

    // The dispatch-time record: batch identity with the inline result
    // pre-filled and the lane call pending, plus a durability ack.
    let (run_id, record, ack) = append_msg(prx.try_recv().expect("batch journaled at dispatch"));
    assert_eq!(run_id, "run-1");
    let RunRecord::ToolBatch {
        calls,
        stage_index,
        iteration,
        response,
        ..
    } = record
    else {
        panic!("expected a ToolBatch record, got {record:?}");
    };
    assert_eq!(stage_index, 0);
    assert_eq!(iteration, agent_state().iteration);
    assert_eq!(response, "r");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id, "c_ctx");
    assert!(calls[0].result.is_some(), "inline result pre-filled");
    assert_eq!(calls[1].id, "c_lane");
    assert_eq!(calls[1].result, None, "lane call pending");
    // Ack the record (standing in for the persistence worker) so the barrier
    // releases immediately instead of timing out.
    ack.expect("dispatch requests an ack").send(()).unwrap();

    // Running the batch reports the lane call's completion as a ToolCallDone.
    let job = jrx.try_recv().expect("lane job enqueued");
    let results = (job.exec)().await;
    assert_eq!(
        results,
        vec![("c_lane".to_string(), "ran read_file".to_string())]
    );
    let (_, record, ack) = append_msg(prx.try_recv().expect("completion journaled"));
    assert!(ack.is_none(), "per-call appends are fire-and-forget");
    let RunRecord::ToolCallDone {
        iteration,
        call_id,
        result,
        ..
    } = record
    else {
        panic!("expected a ToolCallDone record, got {record:?}");
    };
    assert_eq!(iteration, agent_state().iteration);
    assert_eq!(call_id, "c_lane");
    assert_eq!(result, "ran read_file");
}

#[tokio::test]
async fn dispatch_all_inline_batch_is_not_journaled() {
    // A batch the dispatcher fully resolves inline never reaches the lane; its
    // results land in the window, which the snapshot path persists - a batch
    // record would be pure noise.
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let (ptx, mut prx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage::detached(jtx));
    world.insert_resource(PersistenceStage(ptx));
    let e = world
        .spawn((
            agent_state(),
            infer_with(vec![ctx_call("c1", "notes", "hi")]),
            notes_window(),
            StageCursor { index: 0 },
            run_metadata(),
            ReadyForTools,
        ))
        .id();
    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);
    assert!(world.get::<ReadyToInfer>(e).is_some());
    assert!(jrx.try_recv().is_err());
    assert!(prx.try_recv().is_err(), "no batch record for inline-only");
}

#[tokio::test]
async fn dispatch_without_run_metadata_is_unjournaled() {
    // A lane present but no run metadata (an unpersisted agent): the batch
    // dispatches with a no-op progress and nothing is journaled.
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let (ptx, mut prx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(ReportingService)));
    world.insert_resource(ToolStage::detached(jtx));
    world.insert_resource(PersistenceStage(ptx));
    world.spawn((
        agent_state(),
        infer_with(vec![tc("c1", "read_file")]),
        conv_window(),
        ReadyForTools,
    ));
    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    let job = jrx.try_recv().expect("job still enqueued");
    let results = (job.exec)().await;
    assert_eq!(results.len(), 1);
    assert!(prx.try_recv().is_err(), "no journal without run metadata");
}

#[tokio::test]
async fn gate_held_batch_is_not_journaled_until_it_dispatches() {
    // A batch held for a gate prompt has run nothing - journaling it would
    // record calls that may yet be denied. The record is written on the
    // post-resolution re-dispatch instead.
    let (jtx, _jrx) = mpsc::unbounded_channel();
    let (gtx, _grx) = mpsc::unbounded_channel();
    let (ptx, mut prx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage::detached(jtx));
    world.insert_resource(PersistenceStage(ptx));
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
            StageCursor { index: 0 },
            run_metadata(),
            ReadyForTools,
            enabled_gate(),
        ))
        .id();
    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    assert!(
        world
            .get::<crate::gate_prompt::AwaitingGatePrompt>(e)
            .is_some()
    );
    assert!(prx.try_recv().is_err(), "held batch not journaled");
}

#[tokio::test]
async fn barrier_then_runs_after_the_ack() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let exec: BoxedToolExec =
        Box::new(|| Box::pin(async { vec![("c".to_string(), "r".to_string())] }));
    tx.send(()).unwrap();
    let wrapped = barrier_then(exec, rx, std::time::Duration::from_secs(5));
    assert_eq!(wrapped().await, vec![("c".to_string(), "r".to_string())]);
}

#[tokio::test]
async fn barrier_then_proceeds_when_the_sender_is_dropped() {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    drop(tx); // worker gone (shutdown) - the batch must still run
    let exec: BoxedToolExec = Box::new(|| Box::pin(async { Vec::new() }));
    let wrapped = barrier_then(exec, rx, std::time::Duration::from_secs(5));
    assert!(wrapped().await.is_empty());
}

#[tokio::test]
async fn barrier_then_proceeds_on_timeout() {
    // The sender stays alive but never fires (a wedged persistence lane): the
    // bounded wait lapses and the batch runs anyway.
    let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
    let exec: BoxedToolExec = Box::new(|| Box::pin(async { Vec::new() }));
    let wrapped = barrier_then(exec, rx, std::time::Duration::from_millis(5));
    assert!(wrapped().await.is_empty());
}

#[tokio::test]
async fn dispatch_tools_skips_non_active_agent() {
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage::detached(jtx));
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
        fallbacks: Vec::new(),
        output: None,
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
    world.insert_resource(ToolStage::detached(jtx));
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

fn submit_call(id: &str, content: &str) -> crate::components::ToolCall {
    crate::components::ToolCall {
        tool_id: id.to_string(),
        name: leviath_tools::SUBMIT_OUTPUT_TOOL.to_string(),
        arguments: serde_json::json!({ "content": content }),
        thought_signature: None,
    }
}

fn output_window() -> ContextWindow {
    let mut w = conv_window();
    w.add_region(Region::new(
        crate::output_tool::FINAL_OUTPUT_REGION.to_string(),
        RegionKind::Pinned,
        crate::output_tool::FINAL_OUTPUT_REGION_TOKENS,
    ));
    w
}

/// `submit_output` is applied inline for the same reason the context tools are:
/// it writes the live window and an ECS component, neither of which the async
/// tool lane can reach.
#[tokio::test]
async fn dispatch_records_a_submitted_output_inline() {
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage::detached(jtx));
    let e = world
        .spawn((
            agent_state(),
            infer_with(vec![submit_call("o1", "the answer")]),
            output_window(),
            ReadyForTools,
        ))
        .id();

    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    // Nothing reached the lane, and the agent goes back to work rather than
    // ending: no tool in this codebase terminates a run.
    assert!(jrx.try_recv().is_err());
    assert!(world.get::<ReadyToInfer>(e).is_some());

    let recorded = world
        .get::<crate::persistence::FinalOutput>(e)
        .expect("the submission became the run's answer");
    assert_eq!(recorded.0.content, "the answer");
    assert_eq!(recorded.0.stage, agent_state().current_stage);

    // And it is mirrored where the model can see what it committed to.
    assert!(
        world
            .get::<ContextWindow>(e)
            .unwrap()
            .get_region(crate::output_tool::FINAL_OUTPUT_REGION)
            .unwrap()
            .current_tokens
            > 0
    );
}

/// Artifacts are resolved against the run's working directory, so a submission
/// naming one only means something when the agent has a workdir to resolve it
/// in. A path that escapes it is refused, because the answer is handed to a
/// caller who will fetch what it names.
#[tokio::test]
async fn artifacts_are_checked_against_the_run_workdir() {
    let (jtx, _jrx) = mpsc::unbounded_channel();
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("dataset.csv"), "a,b\n1,2\n").expect("write");

    for (artifact, recorded) in [("dataset.csv", true), ("../outside.csv", false)] {
        let mut world = World::new();
        world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
        world.insert_resource(ToolStage::detached(jtx.clone()));
        let call = crate::components::ToolCall {
            tool_id: "o1".to_string(),
            name: leviath_tools::SUBMIT_OUTPUT_TOOL.to_string(),
            arguments: serde_json::json!({
                "content": "the answer",
                "artifacts": [artifact],
            }),
            thought_signature: None,
        };
        let e = world
            .spawn((
                agent_state(),
                infer_with(vec![call]),
                output_window(),
                ReadyForTools,
                RunMetadata {
                    workdir: dir.path().to_string_lossy().to_string(),
                    ..run_metadata()
                },
            ))
            .id();

        let mut s = Schedule::default();
        s.add_systems(dispatch_tools);
        s.run(&mut world);

        assert_eq!(
            world.get::<crate::persistence::FinalOutput>(e).is_some(),
            recorded,
            "artifact {artifact:?}"
        );
    }
}

/// A refused submission must not erase a good answer already recorded. The
/// model correcting itself into something invalid is exactly when the previous
/// answer matters most.
#[tokio::test]
async fn a_refused_submission_leaves_an_earlier_answer_alone() {
    let (jtx, _jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage::detached(jtx));

    // A stage whose answers must be JSON, and a batch that submits a good one
    // and then a bad one.
    let mut offers = offering(&[leviath_tools::SUBMIT_OUTPUT_TOOL]);
    offers.output = Some(leviath_core::output::OutputSpec {
        format: Some("json".to_string()),
        ..leviath_core::output::OutputSpec::default()
    });
    let e = world
        .spawn((
            agent_state(),
            offers,
            crate::components::InferenceResult {
                response: "r".to_string(),
                tool_calls: vec![
                    submit_call("o1", r#"{"answer":"good"}"#),
                    submit_call("o2", "not json at all"),
                ],
                tokens_used: 0,
                timestamp: 0,
            },
            output_window(),
            ReadyForTools,
        ))
        .id();

    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    assert_eq!(
        world
            .get::<crate::persistence::FinalOutput>(e)
            .expect("the good answer survives")
            .0
            .content,
        r#"{"answer":"good"}"#
    );
    // And the model is told why the second one was refused, so it can fix it.
    assert!(
        conversation_text(&world, e).contains("not valid json"),
        "the refusal reaches the model: {}",
        conversation_text(&world, e)
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
        .map(|entry| entry.content.clone())
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
    world.insert_resource(ToolStage::detached(jtx));
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
    world.insert_resource(ToolStage::detached(jtx));
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
    world.insert_resource(ToolStage::detached(jtx));
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
    world.insert_resource(ToolStage::detached(jtx));
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
    world.insert_resource(ToolStage::detached(jtx));
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
    world.insert_resource(ToolStage::detached(jtx));
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
    world.insert_resource(ToolStage::detached(jtx));
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

// ── argument validation (dispatch_tools) ──

/// A stage advertising `tools`, each with a real parameter schema. The plain
/// `offering()` fixture advertises `{}` (accepts anything); these tests are
/// about what happens when a schema actually constrains.
fn offering_with_schemas(tools: &[(&str, serde_json::Value)]) -> StageInference {
    StageInference {
        provider_name: "p".to_string(),
        model: "m".to_string(),
        tools: tools
            .iter()
            .map(|(n, schema)| leviath_providers::Tool {
                name: (*n).to_string(),
                description: String::new(),
                parameters: schema.clone(),
            })
            .collect(),
        tool_filter: None,
        fallbacks: Vec::new(),
        output: None,
    }
}

fn path_required_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": { "path": { "type": "string" } },
        "required": ["path"]
    })
}

/// Layer 2: a call whose arguments do not satisfy the advertised schema is
/// refused back to the model with the validator's message, and never reaches
/// the lane - while a valid call in the same batch still runs.
#[tokio::test]
async fn dispatch_tools_refuses_arguments_that_fail_the_advertised_schema() {
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage::detached(jtx));
    let result = crate::components::InferenceResult {
        response: "r".to_string(),
        tool_calls: vec![
            fcall("c1", "read_file", serde_json::json!({"path": 42})),
            fcall("c2", "read_file", serde_json::json!({"path": "a.txt"})),
        ],
        tokens_used: 0,
        timestamp: 0,
    };
    let e = world
        .spawn((
            agent_state(),
            offering_with_schemas(&[("read_file", path_required_schema())]),
            result,
            conv_window(),
            ReadyForTools,
        ))
        .id();

    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    let stashed = &world.get::<ContextToolResults>(e).unwrap().0;
    assert_eq!(stashed.len(), 1, "only the invalid call was answered here");
    assert_eq!(stashed[0].0, "c1");
    let refusal = stashed[0].1.clone();
    assert!(
        refusal.starts_with("[error] invalid arguments for 'read_file'"),
        "{refusal}"
    );
    // The message names the violation, so the next turn can self-correct.
    assert!(refusal.contains("path"), "{refusal}");

    let job = jrx.try_recv().expect("the valid call still runs");
    assert_eq!(job.entity, e);
}

/// A schema that does not compile (a typo'd Rhai `@param` type produces
/// `{"type": "strng"}`) must not turn its tool unusable: validation is
/// skipped and the call dispatches as before.
#[tokio::test]
async fn dispatch_tools_skips_validation_when_the_schema_does_not_compile() {
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage::detached(jtx));
    let result = crate::components::InferenceResult {
        response: "r".to_string(),
        tool_calls: vec![fcall("c1", "typod", serde_json::json!({"whatever": true}))],
        tokens_used: 0,
        timestamp: 0,
    };
    let e = world
        .spawn((
            agent_state(),
            offering_with_schemas(&[("typod", serde_json::json!({"type": "strng"}))]),
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
    assert!(jrx.try_recv().is_ok(), "the call dispatched anyway");
}

/// The schema lookup resolves aliases on both sides, like the Layer-1 check
/// above it: a stage advertising `bash` constrains a call to `shell`.
#[tokio::test]
async fn dispatch_tools_validates_through_a_tool_alias() {
    let (jtx, _jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage::detached(jtx));
    let canonical = leviath_tools::canonical_tool_name("bash");
    assert_ne!(
        canonical, "bash",
        "this test needs a real alias to be a test"
    );
    let schema = serde_json::json!({
        "type": "object",
        "properties": { "command": { "type": "string" } },
        "required": ["command"]
    });
    let result = crate::components::InferenceResult {
        response: "r".to_string(),
        tool_calls: vec![fcall("c1", canonical, serde_json::json!({}))],
        tokens_used: 0,
        timestamp: 0,
    };
    let e = world
        .spawn((
            agent_state(),
            offering_with_schemas(&[("bash", schema)]),
            result,
            conv_window(),
            ReadyForTools,
        ))
        .id();

    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    let text = conversation_text(&world, e);
    assert!(text.contains("invalid arguments"), "{text}");
    assert!(text.contains("command"), "{text}");
}

/// An MCP-style schema (server-supplied: enums, typed array items) constrains
/// the same way - both directions, accept and refuse.
#[tokio::test]
async fn dispatch_tools_validates_an_mcp_style_schema() {
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage::detached(jtx));
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "mode": { "enum": ["fast", "thorough"] },
            "targets": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["mode"]
    });
    let result = crate::components::InferenceResult {
        response: "r".to_string(),
        tool_calls: vec![
            fcall(
                "c1",
                "mcp_search",
                serde_json::json!({"mode": "sideways", "targets": ["a", 7]}),
            ),
            fcall(
                "c2",
                "mcp_search",
                serde_json::json!({"mode": "fast", "targets": ["a"]}),
            ),
        ],
        tokens_used: 0,
        timestamp: 0,
    };
    let e = world
        .spawn((
            agent_state(),
            offering_with_schemas(&[("mcp_search", schema)]),
            result,
            conv_window(),
            ReadyForTools,
        ))
        .id();

    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    let stashed = &world.get::<ContextToolResults>(e).unwrap().0;
    assert_eq!(stashed.len(), 1);
    assert_eq!(stashed[0].0, "c1");
    assert!(stashed[0].1.contains("/mode"), "{}", stashed[0].1);
    assert!(jrx.try_recv().is_ok(), "the conforming call ran");
}

/// The helper's own edges, directly: no def for the name means nothing to
/// validate (after the unoffered check that cannot happen in dispatch), and
/// the provider's `Null`-for-no-arguments convention satisfies an
/// unconstraining schema.
#[test]
fn invalid_args_refusal_without_a_def_or_constraint_is_none() {
    let stage = offering(&["read_file"]);
    assert_eq!(
        invalid_args_refusal(&stage, "never_advertised", &serde_json::json!({})),
        None
    );
    assert_eq!(
        invalid_args_refusal(&stage, "read_file", &serde_json::Value::Null),
        None
    );
}

/// Every refusal prefix dispatch can produce reads as "this never happened".
/// `[blocked]` was missing until issue #155's pass, so a taint-blocked write
/// counted as a modification.
#[test]
fn call_had_no_effect_covers_every_refusal_prefix() {
    assert!(call_had_no_effect("[error] boom"));
    assert!(call_had_no_effect("[denied] user said no"));
    assert!(call_had_no_effect("[unavailable] not in this stage"));
    assert!(call_had_no_effect("[blocked] taint gate"));
    assert!(!call_had_no_effect("Successfully wrote 12 bytes"));
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
    world.insert_resource(ToolStage::detached(jtx));
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
    world.insert_resource(ToolStage::detached(jtx));
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
    world.insert_resource(ToolStage::detached(jtx));
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
    world.insert_resource(ToolStage::detached(jtx));
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
    world.insert_resource(ToolStage::detached(jtx));
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
    world.insert_resource(ToolStage::detached(jtx));
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
    world.insert_resource(ToolStage::detached(jtx));
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
        tool_max_result_tokens: std::collections::HashMap::new(),
    }
}

// ── Per-tool result ceilings ──

/// The text a tool's result ends up as, after routing applied its ceiling.
fn routed_result(
    routing: &leviath_core::blueprint::ToolResultRouting,
    tool: &str,
    text: &str,
) -> String {
    let mut w = ctx(&[("conversation", 1_000_000), ("results", 1_000_000)]);
    apply_tool_results(
        &mut w,
        "resp",
        &[tc("c1", tool)],
        &[("c1".to_string(), text.to_string())],
        Some(routing),
        None,
    );
    // Whichever region it was routed to, the entry text is what matters here.
    ["results", "conversation", "tool_results"]
        .iter()
        .filter_map(|name| w.get_region(name))
        .flat_map(|r| r.content.iter())
        .map(|e| e.content.clone())
        .find(|c| c.starts_with("aaa"))
        .unwrap_or_default()
}

/// A stage that both greps and reads files cannot express itself with one
/// number: a cap sized for the file read lets a grep through untouched, and one
/// sized for the grep truncates every file.
#[test]
fn a_per_tool_ceiling_overrides_the_stage_one() {
    let mut routing = routing("results", &[], true, Some(10));
    routing
        .tool_max_result_tokens
        .insert("read_file".to_string(), 1000);

    // 400 chars is ~100 tokens: over the stage's 10, under read_file's 1000.
    let text = "a".repeat(400);
    assert!(
        !routed_result(&routing, "read_file", &text).contains("[...truncated]"),
        "the tool's own ceiling should win"
    );
    assert!(
        routed_result(&routing, "grep", &text).contains("[...truncated]"),
        "a tool with no ceiling of its own still gets the stage's"
    );
}

/// Keyed by canonical name, like `tool_overrides`: `bash` is an alias of
/// `shell`, and a literal lookup would silently miss the tool the model calls.
#[test]
fn a_per_tool_ceiling_is_matched_by_canonical_name() {
    let mut routing = routing("results", &[], true, Some(10));
    routing
        .tool_max_result_tokens
        .insert("bash".to_string(), 1000);
    let text = "a".repeat(400);
    assert!(
        !routed_result(&routing, "shell", &text).contains("[...truncated]"),
        "an alias should match the tool it aliases"
    );
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
        .map(|e| e.content.clone())
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
            .any(|e| e.content.contains(&long)),
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
            .any(|e| e.content.contains("FULL FILE BODY"))
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
            .any(|e| e.content.contains("All tests passed")),
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
        fallbacks: Vec::new(),
        output: None,
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
            shell_hint: false,
            request_timeout_secs: None,
        },
        routing: None,
        accepts_messages: true,
        context_layout: None,
        system_prompt: None,
        output: None,
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
fn transition_holds_while_paused_and_resolves_after_resume() {
    // A pause that lands while a ResolveTransition is pending must not be
    // undone by the transition system (entering a stage sets Active).
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
    world.get_mut::<AgentState>(e).unwrap().status = AgentStatus::Paused;

    run_transition(&mut world);

    // Held: still paused, marker intact, cursor unmoved.
    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Paused
    );
    assert!(world.get::<ResolveTransition>(e).is_some());
    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 0);

    // After resume the parked transition resolves normally.
    world.get_mut::<AgentState>(e).unwrap().status = AgentStatus::Active;
    run_transition(&mut world);
    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    assert!(world.get::<ResolveTransition>(e).is_none());
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

// ─── A run that owed an answer and gave none is not complete ─────────────────

/// A stage requiring an output that no stage ever produced.
fn owing_output(require: bool) -> leviath_core::Blueprint {
    let mut stages = vec![stage_named("only", None, false, None)];
    stages[0].require_output = require;
    blueprint(stages)
}

#[test]
fn a_run_that_never_produced_its_required_output_errors() {
    // `require_final_output` forces past the obligation rather than stranding
    // the run - right, since a later stage may still answer - but nothing
    // downgraded the terminal status, so the run reported `complete` with no
    // `final_output` on disk. `lev result` already exited non-zero there, so
    // status and result disagreed in exactly the case a caller most needs.
    let mut world = World::new();
    let e = spawn_transition_agent(
        &mut world,
        owing_output(true),
        vec![si("m0")],
        VisitCounts::default(),
    );

    run_transition(&mut world);

    let status = world.get::<AgentState>(e).unwrap().status.clone();
    let AgentStatus::Error { message } = status else {
        panic!("a run with no answer must not read as success, got {status:?}");
    };
    assert!(message.contains("final output"), "{message}");
}

#[test]
fn a_run_that_produced_its_required_output_completes() {
    let mut world = World::new();
    let e = spawn_transition_agent(
        &mut world,
        owing_output(true),
        vec![si("m0")],
        VisitCounts::default(),
    );
    world.entity_mut(e).insert(crate::persistence::FinalOutput(
        leviath_core::output::FinalOutput {
            stage: "only".to_string(),
            content: "the answer".to_string(),
            format: None,
            submitted_at: 0,
            truncated: false,
            artifacts: Vec::new(),
        },
    ));

    run_transition(&mut world);

    assert!(matches!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Complete
    ));
}

/// A run that never owed one is untouched: this must not turn every ordinary
/// agent into a failure.
#[test]
fn a_run_that_owed_no_output_still_completes() {
    let mut world = World::new();
    let e = spawn_transition_agent(
        &mut world,
        owing_output(false),
        vec![si("m0")],
        VisitCounts::default(),
    );

    run_transition(&mut world);

    assert!(matches!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Complete
    ));
}

#[test]
fn transition_visit_exhausted_edge_is_a_dead_end_error() {
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

    // The stage declared a normal edge and every one of them is exhausted:
    // the graph dead-ended mid-run, which is an ERROR, not a completion.
    // This resolved to `Complete` before, which is how a run silently ended
    // at stage 2 of 5 with the output stage still pending.
    let status = world.get::<AgentState>(e).unwrap().status.clone();
    let AgentStatus::Error { message } = status else {
        panic!("a dead-ended graph must error, got {status:?}");
    };
    assert!(message.contains("dead-ended"), "{message}");
    assert!(message.contains("'a'"), "{message}");
}

// ─── condition = "dead_end" ──────────────────────────────────────────────────

/// The stranding case the condition exists for: the stage finished, every
/// normal target is out of revisits, and the run continues instead of dying
/// with everything it established thrown away.
#[test]
fn a_dead_end_edge_catches_the_strand() {
    use leviath_core::blueprint::TransitionCondition;
    let bp = blueprint(vec![
        stage_named(
            "a",
            Some(vec![
                edge("b", TransitionCondition::Always),
                edge("answer", TransitionCondition::DeadEnd),
            ]),
            false,
            None,
        ),
        stage_named("b", None, false, Some(0)),
        stage_named("answer", None, false, None),
    ]);
    let mut visits = VisitCounts::default();
    visits.0.insert("b".to_string(), 1);
    let mut world = World::new();
    let e = spawn_transition_agent(&mut world, bp, vec![si("m0"), si("m1"), si("m2")], visits);

    run_transition(&mut world);

    let state = world.get::<AgentState>(e).unwrap();
    assert!(
        !matches!(state.status, AgentStatus::Error { .. }),
        "the escape should have been taken, got {:?}",
        state.status
    );
    assert_eq!(
        world.get::<StageCursor>(e).map(|c| c.index),
        Some(2),
        "should have entered the stage the dead_end edge names"
    );
}

/// The whole point of a separate condition: it is *not* a route the model can
/// take while the graph is healthy. An ordinary edge to the same stage is
/// offered on every visit, which is what collapsed the measured pipelines.
#[test]
fn a_dead_end_edge_is_not_offered_while_the_graph_is_healthy() {
    use leviath_core::blueprint::TransitionCondition;
    let bp = blueprint(vec![
        stage_named(
            "a",
            Some(vec![
                edge("b", TransitionCondition::Always),
                edge("answer", TransitionCondition::DeadEnd),
            ]),
            false,
            None,
        ),
        // `b` has budget left this time, so nothing is stranded.
        stage_named("b", None, false, Some(5)),
        stage_named("answer", None, false, None),
    ]);
    let mut world = World::new();
    let e = spawn_transition_agent(
        &mut world,
        bp,
        vec![si("m0"), si("m1"), si("m2")],
        VisitCounts::default(),
    );

    run_transition(&mut world);

    assert_eq!(
        world.get::<StageCursor>(e).map(|c| c.index),
        Some(1),
        "the normal edge should win while it still has budget"
    );
}

/// Both declared: the one written for this situation wins, because an `error`
/// edge is also carrying provider failures and may want to go elsewhere.
#[test]
fn a_dead_end_edge_wins_over_an_error_edge() {
    use leviath_core::blueprint::TransitionCondition;
    let bp = blueprint(vec![
        stage_named(
            "a",
            Some(vec![
                edge("b", TransitionCondition::Always),
                edge("recover", TransitionCondition::Error),
                edge("answer", TransitionCondition::DeadEnd),
            ]),
            false,
            None,
        ),
        stage_named("b", None, false, Some(0)),
        stage_named("recover", None, false, None),
        stage_named("answer", None, false, None),
    ]);
    let mut visits = VisitCounts::default();
    visits.0.insert("b".to_string(), 1);
    let mut world = World::new();
    let e = spawn_transition_agent(
        &mut world,
        bp,
        vec![si("m0"), si("m1"), si("m2"), si("m3")],
        visits,
    );

    run_transition(&mut world);

    assert_eq!(
        world.get::<StageCursor>(e).map(|c| c.index),
        Some(3),
        "the dead_end edge, not the error edge"
    );
}

/// A dead end with an `error` edge in budget routes down it - exhaustion is
/// now a failure mode `error_recovery` can actually catch.
#[test]
fn transition_dead_end_routes_down_the_error_edge_when_present() {
    use leviath_core::blueprint::TransitionCondition;
    let bp = blueprint(vec![
        stage_named(
            "a",
            Some(vec![
                edge("b", TransitionCondition::Always),
                edge("rescue", TransitionCondition::Error),
            ]),
            false,
            None,
        ),
        stage_named("b", None, false, Some(0)), // max_revisits 0
        stage_named("rescue", None, false, None),
    ]);
    let mut visits = VisitCounts::default();
    visits.0.insert("b".to_string(), 1); // b is out of budget
    let mut world = World::new();
    let e = spawn_transition_agent(&mut world, bp, vec![si("m0"), si("m1"), si("m2")], visits);

    run_transition(&mut world);

    let state = world.get::<AgentState>(e).unwrap();
    assert_eq!(state.status, AgentStatus::Active, "recovering, not dead");
    assert_eq!(state.current_stage, "rescue");
    // And the recovery stage can read why it was entered.
    let window = world.get::<ContextWindow>(e).unwrap();
    let noted = window
        .regions
        .iter()
        .flat_map(|r| r.content.iter())
        .any(|entry| entry.content.contains("dead-ended"));
    assert!(noted, "the dead-end reason is in the context");
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
fn transition_unknown_target_edge_is_a_dead_end_error() {
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

    // The only declared edge points at a nonexistent stage: nothing can ever
    // follow it, so completing here would be a silent lie.
    let status = world.get::<AgentState>(e).unwrap().status.clone();
    let AgentStatus::Error { message } = status else {
        panic!("an unfollowable graph must error, got {status:?}");
    };
    assert!(message.contains("dead-ended"), "{message}");
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
        shell_hint: false,
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
    assert!(w.get_region("scratch").is_some(), "the stage's own region");
    // The region the stage did not declare is HELD, not dropped. It used to be
    // deleted, which made a per-stage layout unusable for narrowing a view in a
    // pipeline whose later stages still need the data: re-declaring it
    // downstream brought it back empty.
    assert!(
        w.get_region("sys").is_some(),
        "an omitted region must survive the stage it is not shown to"
    );
    assert!(
        w.hidden.contains("sys"),
        "and it must not be assembled into this stage's prompt"
    );
}

/// The point of holding it: a later stage that declares it again gets its
/// contents back, rather than an empty region.
#[test]
fn a_region_hidden_by_one_stage_comes_back_with_its_content() {
    use leviath_core::layout::{ContextLayout, RegionDefinition};

    let mut w = pinned_window();
    w.add_to_region("sys", "the data preview".to_string(), 4)
        .expect("seeded");

    // A stage that does not declare `sys`.
    crate::context_setup::apply_layout(
        &mut w,
        &ContextLayout::new(
            vec![RegionDefinition::new(
                "scratch".to_string(),
                RegionKind::Clearable,
                5000,
            )],
            8000,
        ),
    );
    assert!(w.hidden.contains("sys"));
    assert!(
        w.get_region("sys").is_some_and(|r| !r.content.is_empty()),
        "held with its content while hidden"
    );

    // A later stage that declares it again.
    crate::context_setup::apply_layout(
        &mut w,
        &ContextLayout::new(
            vec![RegionDefinition::new(
                "sys".to_string(),
                RegionKind::Pinned,
                5000,
            )],
            8000,
        ),
    );
    assert!(!w.hidden.contains("sys"), "declared again, so shown again");
    let restored = w.get_region("sys").expect("still there");
    assert_eq!(
        restored.content.first().map(|e| e.content.as_str()),
        Some("the data preview"),
        "and it is the same content, not an empty region with the same name"
    );
}

/// A hidden region is held but does not reach the model.
///
/// The other half of the contract: holding it would be pointless if it were
/// still assembled, and hiding it would be data loss if it were dropped.
#[test]
fn a_hidden_region_is_not_assembled_into_the_prompt() {
    let mut w = ContextWindow::new(10_000);
    w.add_region(Region::new("notes".to_string(), RegionKind::Pinned, 5000));
    w.add_to_region("notes", "SECRET-MARKER".to_string(), 4)
        .expect("seeded");

    let meta = crate::custom_region::AssembleMeta::default();
    let visible = w.assemble_with_meta(&meta);
    assert!(
        format!("{visible:?}").contains("SECRET-MARKER"),
        "precondition: it assembles while visible"
    );

    w.hidden.insert("notes".to_string());
    let hidden = w.assemble_with_meta(&meta);
    assert!(
        !format!("{hidden:?}").contains("SECRET-MARKER"),
        "a region this stage does not attend to must not reach the model"
    );
    assert!(
        w.get_region("notes").is_some_and(|r| !r.content.is_empty()),
        "and it is still held"
    );
}

/// The message-stream regions are carried *visible* even when a stage omits
/// them: hiding `conversation` would strand a history the next stage's own
/// typed turns have to attach to.
#[test]
fn the_message_regions_are_never_hidden() {
    use leviath_core::layout::{ContextLayout, RegionDefinition};

    let mut w = ContextWindow::new(10_000);
    w.add_region(Region::new(
        "conversation".to_string(),
        RegionKind::SlidingWindow {
            max_items: 10,
            eviction_strategy: leviath_core::EvictionStrategy::PerItem,
        },
        5000,
    ));
    w.add_region(Region::new("notes".to_string(), RegionKind::Pinned, 5000));

    crate::context_setup::apply_layout(
        &mut w,
        &ContextLayout::new(
            vec![RegionDefinition::new(
                "scratch".to_string(),
                RegionKind::Clearable,
                5000,
            )],
            8000,
        ),
    );

    assert!(!w.hidden.contains("conversation"));
    assert!(w.hidden.contains("notes"));
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
        fallbacks: Vec::new(),
        output: None,
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
        hints(true),
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
        hints(true),
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
            results_region: None,
            max_items: None,
        },
    };

    // Fan-out stage with a base prompt: split prompt is appended.
    let mut s = stage_named("fan", None, false, None);
    s.mode = fanout("SPLIT NOW");
    s.config.insert(
        "system_prompt".to_string(),
        serde_json::Value::String("base instructions".to_string()),
    );
    let sp = stage_setup_from(&s, hints(true), Default::default(), None)
        .system_prompt
        .unwrap();
    assert!(sp.contains("base instructions") && sp.contains("SPLIT NOW"));

    // Fan-out stage with no base prompt: the split prompt alone.
    let mut s2 = stage_named("fan", None, false, None);
    s2.mode = fanout("ONLY SPLIT");
    assert_eq!(
        stage_setup_from(&s2, hints(true), Default::default(), None).system_prompt,
        Some("ONLY SPLIT".to_string())
    );

    // Fan-out stage with an empty split prompt: base prompt is left as-is.
    let mut s3 = stage_named("fan", None, false, None);
    s3.mode = fanout("   ");
    assert_eq!(
        stage_setup_from(&s3, hints(true), Default::default(), None).system_prompt,
        None
    );
}

#[test]
fn stage_setup_from_cascades_each_hint_independently() {
    use leviath_core::config::{PromptHintOverrides, PromptHints};

    // Globals on, agent silent, stage silent → both inherit on.
    let s = stage_named("plan", None, false, None);
    let cfg =
        stage_setup_from(&s, hints(true), PromptHintOverrides::default(), None).inference_config;
    assert!(cfg.batch_tool_hint);
    assert!(cfg.shell_hint);

    // The agent level opts out of one hint without touching the other.
    let agent_off_shell = PromptHintOverrides {
        batch_tool: None,
        shell: Some(false),
    };
    let cfg = stage_setup_from(&s, hints(true), agent_off_shell, None).inference_config;
    assert!(cfg.batch_tool_hint);
    assert!(!cfg.shell_hint);

    // The stage level wins over the agent, in both directions at once.
    let mut s2 = stage_named("plan", None, false, None);
    s2.shell_hint = Some(true);
    s2.batch_tool_hint = Some(false);
    let cfg = stage_setup_from(
        &s2,
        PromptHints {
            batch_tool: true,
            shell: false,
        },
        agent_off_shell,
        None,
    )
    .inference_config;
    assert!(!cfg.batch_tool_hint);
    assert!(cfg.shell_hint);
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

    let setup = stage_setup_from(&s, hints(true), Default::default(), None);
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
        stage_setup_from(&s, hints(true), Default::default(), None)
            .inference_config
            .request_timeout_secs,
        None
    );

    // Set on the stage's model → carried onto the inference config verbatim.
    let mut s2 = stage_named("plan", None, false, None);
    s2.model.request_timeout_secs = Some(300);
    assert_eq!(
        stage_setup_from(&s2, hints(true), Default::default(), None)
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
        hints(true),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_compaction_job_reports_an_error_instead_of_vanishing() {
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    register_exploding(&mut world);
    let (ctx, mut crx) = mpsc::unbounded_channel();
    world.resource_mut::<InferenceStage>().compaction_outcomes = ctx;
    let e = world
        .spawn((
            compacting_window(),
            compaction_settings("exploding", "m"),
            agent_state(),
            ReadyToInfer,
        ))
        .id();

    let _silent = crate::test_support::SilentPanics::install();
    run_dispatch_compaction(&mut world);

    // Compaction is best-effort, but *waiting* for it is not: the agent is held
    // `AwaitingCompaction` until an outcome lands.
    assert!(world.get::<AwaitingCompaction>(e).is_some());
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), crx.recv())
        .await
        .expect("the supervisor reports promptly")
        .expect("an outcome");
    assert_eq!(outcome.entity, e);
    let err = outcome
        .result
        .expect_err("a dead job is an error")
        .to_string();
    assert!(err.contains("compaction"), "got: {err}");
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
    let _ = sys.add_entry("identity".to_string(), 10);
    w.add_region(sys);
    let mut scratch = Region::new("scratch".to_string(), RegionKind::Clearable, 500);
    let _ = scratch.add_entry("work".to_string(), 10);
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
    let _ = scratch_custom.add_entry("wipe me".to_string(), 10);
    w.add_region(scratch_custom);
    let mut vault = Region::new(
        "vault".to_string(),
        RegionKind::Custom {
            script: "v.rhai".to_string(),
            persistent: true,
        },
        500,
    );
    let _ = vault.add_entry("keep me".to_string(), 10);
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
    let _ = keep.add_entry("keepme".to_string(), 10);
    w.add_region(keep);
    let mut drop = Region::new("drop".to_string(), RegionKind::Clearable, 500);
    let _ = drop.add_entry("dropme".to_string(), 10);
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
    let _ = scratch.add_entry("work to summarize".to_string(), 20);
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
    let text: String = conv.content.iter().map(|e| e.content.as_str()).collect();
    assert!(
        text.contains("Stuck detected in stage 'implement'"),
        "{text}"
    );
    assert!(text.contains("you are looping"), "{text}");
}

#[test]
fn note_error_prefers_the_error_report_region_then_conversation() {
    let mut with_report = ctx(&[("conversation", 10_000), ("error_report", 10_000)]);
    note_error(&mut with_report, "gather", "provider timed out");
    assert!(
        with_report
            .get_region("error_report")
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

    // Blueprints that declare no error_report still get the error text -
    // every blueprint is required to declare `conversation`.
    let mut fallback = ctx(&[("conversation", 10_000)]);
    note_error(&mut fallback, "gather", "provider timed out");
    let conv = fallback.get_region("conversation").unwrap();
    let text: String = conv.content.iter().map(|e| e.content.as_str()).collect();
    assert!(text.contains("Inference error in stage 'gather'"), "{text}");
    assert!(text.contains("provider timed out"), "{text}");
}

#[test]
fn note_max_iterations_prefers_the_error_report_region_then_conversation() {
    let mut with_report = ctx(&[("conversation", 10_000), ("error_report", 10_000)]);
    note_max_iterations(&mut with_report, "implement", 12);
    assert!(
        with_report
            .get_region("error_report")
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

    let mut fallback = ctx(&[("conversation", 10_000)]);
    note_max_iterations(&mut fallback, "implement", 12);
    let conv = fallback.get_region("conversation").unwrap();
    let text: String = conv.content.iter().map(|e| e.content.as_str()).collect();
    assert!(
        text.contains("Stage 'implement' hit its iteration cap (12)"),
        "{text}"
    );
    assert!(text.contains("possibly incomplete"), "{text}");
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
        conv.content.iter().any(|c| c.content.contains("where.py")),
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
    // The error text was written into context for the recovery stage to read.
    let text = conversation_text(&world, e);
    assert!(
        text.contains("[Inference error in stage 'a'] boom"),
        "{text}"
    );
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
    // A terminal error writes no note - the run is over and the status
    // already carries the message.
    assert_eq!(conversation_text(&world, e), "");
}

#[test]
fn resolve_transition_routes_max_iterations_edge_else_falls_through() {
    // With a max_iterations edge → follow it.
    let mi = conditioned_edge("recovery", TransitionCondition::MaxIterations);
    let mut a = stage_named("a", Some(vec![("m".to_string(), mi)]), false, None);
    a.max_iterations = Some(7);
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
    // The cap note reached context so the recovery stage knows why it runs.
    let text = conversation_text(&world, e);
    assert!(
        text.contains("Stage 'a' hit its iteration cap (7)"),
        "{text}"
    );

    // Without one → fall through to a normal (linear) transition, still
    // telling the next stage the work was cut off.
    let mut a2 = stage_named("a", None, false, None);
    a2.max_iterations = Some(3);
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
    let text2 = conversation_text(&world2, e2);
    assert!(
        text2.contains("Stage 'a' hit its iteration cap (3)"),
        "{text2}"
    );
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
        .map(|entry| entry.content.clone())
        .collect::<String>();
    assert!(conv.contains("Required context region 'plan' is still empty"));
}

#[test]
fn require_context_regions_interpolates_a_custom_message() {
    // A custom required_message may name its region via {region} - the same
    // substitution the generated default goes through.
    let mut world = World::new();
    let e = world
        .spawn((
            required_bp(
                &["context_write"],
                Some("Write {region} with context_write before finishing."),
            ),
            StageCursor { index: 0 },
            window_with_plan(false),
            ResolveTransition,
        ))
        .id();
    run_require(&mut world);
    assert!(
        conversation_text(&world, e)
            .contains("[System] Write plan with context_write before finishing.")
    );
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

// ── transition gates: require_region_updated (#343) ──

/// A gate that watches a region for change rather than for content.
fn change_gate(region: &str) -> leviath_core::blueprint::TransitionGate {
    leviath_core::blueprint::TransitionGate {
        require_region_updated: Some(region.to_string()),
        ..Default::default()
    }
}

/// A window holding `plan` with the given text.
fn plan_window(text: &str) -> ContextWindow {
    let mut w = ContextWindow::new(10_000);
    w.add_region(Region::new("plan".to_string(), RegionKind::Pinned, 5000));
    if !text.is_empty() {
        w.add_to_region("plan", text.to_string(), 4)
            .expect("seeded");
    }
    w
}

/// Progress whose baseline is the plan as it stood on entry.
fn progress_with_baseline(w: &ContextWindow) -> StageProgress {
    let mut p = StageProgress::default();
    if let Some(region) = w.get_region("plan") {
        p.entry_region_digests.insert(
            "plan".to_string(),
            crate::pipeline::transition::region_digest(region),
        );
    }
    p
}

#[test]
fn an_unchanged_region_blocks_the_edge() {
    // The failure this exists for: a stage sent back to revise satisfies every
    // other gate by re-emitting what it already wrote, so a reviewer's
    // rejection can be answered with the same plan. Measured, a plan that
    // overrode a documented definition was re-confirmed and the run ended
    // confidently wrong.
    let w = plan_window("fraud = largest count");
    let progress = progress_with_baseline(&w);
    let stage = stage_named("plan", None, false, None);

    let decision = gate_blocks(Some(&change_gate("plan")), &stage, &progress, &w);
    let GateDecision::Block(nudge) = decision else {
        panic!("an unchanged plan must not pass, got {decision:?}");
    };
    assert!(nudge.contains("plan"), "{nudge}");
}

#[test]
fn a_changed_region_passes() {
    let before = plan_window("fraud = largest count");
    let progress = progress_with_baseline(&before);

    // The same stage, having actually revised the plan.
    let after = plan_window("fraud = fraudulent volume / total volume");
    let stage = stage_named("plan", None, false, None);

    assert!(matches!(
        gate_blocks(Some(&change_gate("plan")), &stage, &progress, &after),
        GateDecision::Pass
    ));
}

/// A gate naming a region the window does not hold cannot be satisfied by any
/// amount of work, so it passes rather than stranding the run.
#[test]
fn a_gate_on_a_missing_region_passes() {
    let w = ContextWindow::new(10_000);
    let stage = stage_named("plan", None, false, None);
    assert!(matches!(
        gate_blocks(
            Some(&change_gate("nope")),
            &stage,
            &StageProgress::default(),
            &w
        ),
        GateDecision::Pass
    ));
}

/// It shares the one re-run budget every other gate uses: a gate that could
/// hold a stage forever would strand the run.
#[test]
fn an_unchanged_region_gives_up_after_the_budget() {
    let w = plan_window("unchanged");
    let mut progress = progress_with_baseline(&w);
    progress.gate_reentries = leviath_core::blueprint::DEFAULT_GATE_ATTEMPTS;
    let stage = stage_named("plan", None, false, None);

    assert!(matches!(
        gate_blocks(Some(&change_gate("plan")), &stage, &progress, &w),
        GateDecision::Forced
    ));
}

/// The author's own wording wins, as it does for every other gate.
#[test]
fn a_custom_message_is_used() {
    let w = plan_window("unchanged");
    let progress = progress_with_baseline(&w);
    let stage = stage_named("plan", None, false, None);
    let mut gate = change_gate("plan");
    gate.message = Some("The check rejected this plan.".to_string());

    let GateDecision::Block(nudge) = gate_blocks(Some(&gate), &stage, &progress, &w) else {
        panic!("should block");
    };
    assert_eq!(nudge, "The check rejected this plan.");
}

/// The baseline is taken only for the regions a gate actually watches.
///
/// Hashing every region on every stage entry would cost the whole window for a
/// feature most stages do not use, so the collector is selective - and that
/// selectivity is what these four cases pin.
#[test]
fn only_watched_regions_get_a_baseline() {
    use leviath_core::blueprint::TransitionCondition;

    let w = plan_window("the plan");

    // No transitions at all.
    let bare = stage_named("plan", None, false, None);
    assert!(crate::pipeline::transition::watched_region_digests(&bare, &w).is_empty());

    // An edge with no gate.
    let ungated = stage_named(
        "plan",
        Some(vec![edge("compute", TransitionCondition::Always)]),
        false,
        None,
    );
    assert!(crate::pipeline::transition::watched_region_digests(&ungated, &w).is_empty());

    // An edge whose gate watches a region the window holds.
    let mut watching_edge = edge("compute", TransitionCondition::Always);
    watching_edge.1.gate = Some(change_gate("plan"));
    let watching = stage_named("plan", Some(vec![watching_edge]), false, None);
    let digests = crate::pipeline::transition::watched_region_digests(&watching, &w);
    assert_eq!(digests.len(), 1);
    assert!(digests.contains_key("plan"));

    // And one that watches a region it does not hold: no baseline, which the
    // gate reads as "cannot demand an update to something absent".
    let mut missing_edge = edge("compute", TransitionCondition::Always);
    missing_edge.1.gate = Some(change_gate("nope"));
    let missing = stage_named("plan", Some(vec![missing_edge]), false, None);
    assert!(crate::pipeline::transition::watched_region_digests(&missing, &w).is_empty());
}

// ── transition gates: require_modifications (#107) ──

fn gate(region: Option<&str>, message: Option<&str>) -> leviath_core::blueprint::TransitionGate {
    leviath_core::blueprint::TransitionGate {
        require_modifications: true,
        message: message.map(str::to_string),
        region: region.map(str::to_string),
        tools: Vec::new(),
        max_attempts: None,
        require_region_updated: None,
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
        .map(|entry| entry.content.clone())
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

/// A taint-blocked write never ran either. `[blocked]` was missing from the
/// no-effect prefixes, so it counted as a successful modification - a stage
/// whose every write the gate stopped could still satisfy a
/// `require_modifications` transition.
#[test]
fn collect_tools_ignores_a_write_the_taint_gate_blocked() {
    let (progress, flags) = count_modifications(
        &[(
            "write_file",
            serde_json::json!({"path": "exfil.txt"}),
            "[blocked] 'write_file' would carry Internal-tainted data.",
        )],
        &[],
    );
    assert_eq!(progress.modifying_tool_calls, 0);
    // Not "blocked_modification_calls" - that counter is the user declining;
    // the gate refusing is not the agent having tried and been overruled.
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
        .map(|entry| entry.content.clone())
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
        unattended: false,
        read_paths: None,
        output_request: None,
    }
}

fn world_with_persistence() -> (World, mpsc::UnboundedReceiver<PersistMsg>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(PersistenceStage(tx));
    (world, rx)
}

/// Unwrap the snapshot job a dispatch-persistence test expects on the lane.
fn snapshot_job(msg: PersistMsg) -> PersistJob {
    match msg {
        PersistMsg::Snapshot(job) => *job,
        PersistMsg::Append { .. } | PersistMsg::StageLines { .. } => {
            panic!("expected a snapshot on the lane")
        }
    }
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

/// A stage holding for its sub-agents owns its own `Waiting`. Reflection must
/// not touch it: the clearing arm would otherwise walk the parent back to
/// `Active` the moment an unrelated prompt of its own resolved, un-parking a run
/// whose children are still going.
#[test]
fn reflect_leaves_an_agent_waiting_on_its_children_alone() {
    let hub = InteractionHub::new(); // empty ⇒ nothing pending
    let mut world = World::new();
    world.insert_resource(hub);
    let e = world
        .spawn((
            reflect_state("a", AgentStatus::Waiting),
            AwaitingInteraction,
            crate::pipeline::WaitingForChildren,
        ))
        .id();

    run_reflect(&mut world);

    assert_eq!(
        world.get::<AgentState>(e).unwrap().status,
        AgentStatus::Waiting,
        "the children are still running; nothing has un-parked this stage"
    );
    assert!(
        world.get::<AwaitingInteraction>(e).is_some(),
        "the query skipped this agent entirely, marker included"
    );
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
    let job = snapshot_job(rx.try_recv().expect("first snapshot written"));
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
    let job = snapshot_job(rx.try_recv().expect("second snapshot after change"));
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
    let job = snapshot_job(rx.try_recv().expect("snapshot after completion"));
    assert_eq!(job.meta.status, leviath_core::run_meta::RunStatus::Complete);
}

/// `last_progress_at` is what `lev ps` ages its rows against, so it must move
/// only when the agent does. `updated_at` cannot serve: the heartbeat advances
/// it on a run that is doing nothing at all, which is exactly how a wedged run
/// gets mistaken for a busy one (issue #184).
#[test]
fn last_progress_at_tracks_progress_and_not_the_heartbeat() {
    let (mut world, mut rx) = world_with_persistence();
    let e = spawn_persistable(&mut world);

    run_dispatch_persistence(&mut world);
    let job = snapshot_job(rx.try_recv().expect("first snapshot"));
    let first = world
        .get::<PersistWatermark>(e)
        .unwrap()
        .last_progress_at()
        .expect("the first snapshot is progress");
    assert_eq!(
        job.meta.last_progress_at,
        Some(first),
        "the stamp reaches meta.json, where a harness can read it"
    );

    // Backdate both stamps past the heartbeat window, then dispatch with the
    // agent unchanged: a beat is written, but nothing moved.
    let stale = first - (PERSIST_HEARTBEAT_SECS + 5);
    world
        .get_mut::<PersistWatermark>(e)
        .unwrap()
        .backdate(stale);
    run_dispatch_persistence(&mut world);
    let beat = snapshot_job(
        rx.try_recv()
            .expect("the heartbeat still writes a snapshot"),
    );
    assert_eq!(
        world.get::<PersistWatermark>(e).unwrap().last_progress_at(),
        Some(stale),
        "a heartbeat is not progress"
    );
    // The two timestamps diverge, which is the whole point: `updated_at` says
    // the daemon is alive, `last_progress_at` says the run is not moving.
    assert_eq!(
        beat.meta.last_progress_at,
        Some(stale),
        "a heartbeat-only write must not advance the progress stamp"
    );
    assert!(
        beat.meta.updated_at > stale,
        "the heartbeat does advance updated_at, which is why it cannot be trusted as progress"
    );

    // A real iteration does move it.
    world.get_mut::<AgentState>(e).unwrap().iteration += 1;
    run_dispatch_persistence(&mut world);
    let moved = snapshot_job(rx.try_recv().expect("snapshot after real progress"));
    let progressed = world
        .get::<PersistWatermark>(e)
        .unwrap()
        .last_progress_at()
        .expect("still stamped");
    assert!(progressed > stale, "a new iteration is progress");
    assert_eq!(
        moved.meta.last_progress_at,
        Some(moved.meta.updated_at),
        "a write that carried progress stamps both with the same instant"
    );
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
        fallbacks: Vec::new(),
        output: None,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_routing_job_reports_an_error_instead_of_vanishing() {
    let (mut world, _rx) = build_world(InferencePools::new(InferencePoolConfig::new()));
    register_exploding(&mut world);
    let (ttx, mut trx) = mpsc::unbounded_channel();
    world.resource_mut::<InferenceStage>().transition_outcomes = ttx;

    let bp = blueprint(vec![stage_named("a", None, false, None)]);
    let e = spawn_choosing_agent(&mut world, bp, vec![si("m0")], vec![plain_edge("a")]);
    world
        .entity_mut(e)
        .insert(stage_infs_head().clone_with_provider("exploding"));

    let _silent = crate::test_support::SilentPanics::install();
    let mut schedule = Schedule::default();
    schedule.add_systems(dispatch_transition_choice);
    schedule.run(&mut world);

    // Parked on `AwaitingTransitionResponse`: without an outcome the agent is
    // stranded mid-route, having already left its stage behind.
    assert!(world.get::<AwaitingTransitionResponse>(e).is_some());
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), trx.recv())
        .await
        .expect("the supervisor reports promptly")
        .expect("an outcome");
    assert_eq!(outcome.entity, e);
    let err = outcome
        .result
        .expect_err("a dead job is an error")
        .to_string();
    assert!(err.contains("transition-choice"), "got: {err}");
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
        fallbacks: Vec::new(),
        output: None,
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
                fallbacks: Vec::new(),
                output: None,
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

// ── source-emitted world events (stage transitions + tool calls) ──

#[test]
fn resolve_transition_emits_a_stage_transition_event() {
    use crate::host::{WorldEvent, WorldEventSink};
    let bp = blueprint(vec![
        stage_named("a", None, false, None),
        stage_named("b", None, false, None),
    ]);
    let mut world = World::new();
    let (sink_tx, mut sink_rx) = tokio::sync::broadcast::channel(16);
    world.insert_resource(WorldEventSink(sink_tx));
    let e = spawn_transition_agent(
        &mut world,
        bp,
        vec![stage("m", vec![], None), stage("m", vec![], None)],
        VisitCounts::default(),
    );
    world.entity_mut(e).insert(run_metadata());

    run_transition(&mut world);

    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    let ev = sink_rx.try_recv().expect("stage transition event");
    assert_eq!(
        ev,
        WorldEvent::StageTransition {
            run_id: "run-1".to_string(),
            agent_id: "a".to_string(),
            from: "s".to_string(), // the fixture agent's starting stage name
            to: "b".to_string(),
            iteration: 1,
        }
    );
    assert!(sink_rx.try_recv().is_err(), "exactly one event");
}

#[test]
fn stage_transition_event_needs_run_metadata() {
    use crate::host::WorldEventSink;
    // A sink is installed but the agent carries no RunMetadata (a bare test
    // agent): the transition happens, the stream stays silent.
    let bp = blueprint(vec![
        stage_named("a", None, false, None),
        stage_named("b", None, false, None),
    ]);
    let mut world = World::new();
    let (sink_tx, mut sink_rx) = tokio::sync::broadcast::channel(16);
    world.insert_resource(WorldEventSink(sink_tx));
    let e = spawn_transition_agent(
        &mut world,
        bp,
        vec![stage("m", vec![], None), stage("m", vec![], None)],
        VisitCounts::default(),
    );

    run_transition(&mut world);

    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    assert!(sink_rx.try_recv().is_err(), "no event without metadata");
}

#[test]
fn collect_choice_emits_a_stage_transition_event() {
    use crate::host::{WorldEvent, WorldEventSink};
    let (mut world, tx) = world_with_transition_results();
    let (sink_tx, mut sink_rx) = tokio::sync::broadcast::channel(16);
    world.insert_resource(WorldEventSink(sink_tx));
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
    world.entity_mut(e).insert(run_metadata());
    tx.send(InferenceOutcome {
        latency: std::time::Duration::ZERO,
        entity: e,
        result: Ok(resp("b")),
    })
    .unwrap();

    run_collect_transition(&mut world);

    assert_eq!(world.get::<StageCursor>(e).unwrap().index, 1);
    let ev = sink_rx.try_recv().expect("stage transition event");
    assert_eq!(
        ev,
        WorldEvent::StageTransition {
            run_id: "run-1".to_string(),
            agent_id: "a".to_string(),
            from: "s".to_string(),
            to: "b".to_string(),
            iteration: 1,
        }
    );
}

#[tokio::test]
async fn dispatch_tools_announces_lane_calls() {
    use crate::host::{WorldEvent, WorldEventSink};
    let (jtx, mut jrx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolServiceRes(Arc::new(EchoService)));
    world.insert_resource(ToolStage::detached(jtx));
    let (sink_tx, mut sink_rx) = tokio::sync::broadcast::channel(16);
    world.insert_resource(WorldEventSink(sink_tx));
    let e = world
        .spawn((
            agent_state(),
            infer_result(true),
            conv_window(),
            ReadyForTools,
            run_metadata(),
        ))
        .id();

    let mut s = Schedule::default();
    s.add_systems(dispatch_tools);
    s.run(&mut world);

    assert!(world.get::<AwaitingTools>(e).is_some());
    let _ = jrx.try_recv().expect("job enqueued");
    let ev = sink_rx.try_recv().expect("tool call started event");
    assert_eq!(
        ev,
        WorldEvent::ToolCallStarted {
            run_id: "run-1".to_string(),
            agent_id: "a".to_string(),
            call_id: "t".to_string(),
            tool: "n".to_string(),
        }
    );
    assert!(sink_rx.try_recv().is_err(), "one event per lane call");
}

#[test]
fn collect_tools_reports_finished_lane_calls() {
    use crate::host::{WorldEvent, WorldEventSink};
    let (tx, rx) = mpsc::unbounded_channel();
    let mut world = World::new();
    world.insert_resource(ToolResults(rx));
    let (sink_tx, mut sink_rx) = tokio::sync::broadcast::channel(16);
    world.insert_resource(WorldEventSink(sink_tx));
    let e = world
        .spawn((
            ctx(&[("conversation", 10_000)]),
            infer_with(vec![tc("c1", "read"), tc("c2", "write")]),
            agent_state(),
            run_metadata(),
            AwaitingTools,
        ))
        .id();
    tx.send(ToolOutcome {
        elapsed: std::time::Duration::ZERO,
        entity: e,
        // A success, a failure, and a result whose id matches no known call
        // (the tool name falls back to empty rather than panicking).
        results: vec![
            ("c1".to_string(), "file body".to_string()),
            ("c2".to_string(), "[error] denied".to_string()),
            ("zz".to_string(), "stray".to_string()),
        ],
    })
    .unwrap();

    run_collect_tools(&mut world);

    assert_eq!(
        sink_rx.try_recv().expect("first finish"),
        WorldEvent::ToolCallFinished {
            run_id: "run-1".to_string(),
            agent_id: "a".to_string(),
            call_id: "c1".to_string(),
            tool: "read".to_string(),
            ok: true,
            summary: "file body".to_string(),
        }
    );
    assert_eq!(
        sink_rx.try_recv().expect("second finish"),
        WorldEvent::ToolCallFinished {
            run_id: "run-1".to_string(),
            agent_id: "a".to_string(),
            call_id: "c2".to_string(),
            tool: "write".to_string(),
            ok: false,
            summary: "[error] denied".to_string(),
        }
    );
    assert_eq!(
        sink_rx.try_recv().expect("stray finish"),
        WorldEvent::ToolCallFinished {
            run_id: "run-1".to_string(),
            agent_id: "a".to_string(),
            call_id: "zz".to_string(),
            tool: String::new(),
            ok: true,
            summary: "stray".to_string(),
        }
    );
    assert!(sink_rx.try_recv().is_err(), "no extra events");
}

// ── Final-output shape reaches the model ─────────────────────────────────────

/// A required output is stated in the stage's own instructions, on top of the
/// tool description carrying the same shape. Both, because a format the model
/// has no prior knowledge of is exactly where one mention is easy to miss.
#[test]
fn stage_setup_from_folds_a_required_output_into_the_system_prompt() {
    let spec = leviath_core::output::OutputSpec {
        format: Some("a2ui".to_string()),
        instructions: Some("One card per finding.".to_string()),
        example: Some("{\"root\": {}}".to_string()),
        schema: None,
        validator: None,
    };
    let mut s = stage_named("summary", None, false, None);
    s.require_output = true;
    s.config.insert(
        "system_prompt".to_string(),
        serde_json::Value::String("base instructions".to_string()),
    );
    let prompt = stage_setup_from(&s, hints(true), Default::default(), Some(spec))
        .system_prompt
        .expect("a required output always produces instructions");
    assert!(prompt.contains("base instructions"), "{prompt}");
    assert!(prompt.contains("submit_output"), "{prompt}");
    // The unrecognized format and its example are pasted through verbatim;
    // nothing here knows what a2ui is.
    assert!(prompt.contains("a2ui"), "{prompt}");
    assert!(prompt.contains("One card per finding."), "{prompt}");
    assert!(prompt.contains("{\"root\": {}}"), "{prompt}");
}

/// Issue #282. The stage's own prompt and the resolved spec are both just text
/// to a model, so the spec has to come last *and* say it governs. Ordering on
/// its own is what the reported build already did, and a strong stage prompt
/// still won on some models.
#[test]
fn a_required_outputs_shape_comes_after_the_stage_prompt_and_outranks_it() {
    let spec = leviath_core::output::OutputSpec {
        format: Some("text".to_string()),
        instructions: Some("Reply with only the integer.".to_string()),
        ..Default::default()
    };
    let mut s = stage_named("summary", None, false, None);
    s.require_output = true;
    s.config.insert(
        "system_prompt".to_string(),
        serde_json::Value::String("Lead with the diagnosis.".to_string()),
    );
    let prompt = stage_setup_from(&s, hints(true), Default::default(), Some(spec))
        .system_prompt
        .expect("a required output always produces instructions");
    let stage_at = prompt
        .find("Lead with the diagnosis.")
        .expect("stage prompt");
    let caller_at = prompt
        .find("Reply with only the integer.")
        .expect("the caller's instructions");
    let rule_at = prompt
        .find("Where anything else you were told")
        .expect("the precedence rule");
    assert!(stage_at < caller_at, "{prompt}");
    assert!(caller_at < rule_at, "{prompt}");
}

/// A stage that declares a shape but is not required to submit is left alone:
/// declaring is not demanding.
#[test]
fn stage_setup_from_leaves_an_unrequired_stage_prompt_alone() {
    let spec = leviath_core::output::OutputSpec {
        format: Some("markdown".to_string()),
        ..Default::default()
    };
    let mut s = stage_named("plan", None, false, None);
    s.config.insert(
        "system_prompt".to_string(),
        serde_json::Value::String("base instructions".to_string()),
    );
    let prompt = stage_setup_from(&s, hints(true), Default::default(), Some(spec))
        .system_prompt
        .expect("the base prompt survives");
    assert_eq!(prompt, "base instructions");
}

/// A required output with nothing declared about its shape still says it is
/// required, since that is the part the agent must act on.
#[test]
fn stage_setup_from_demands_an_output_even_with_no_declared_shape() {
    let mut s = stage_named("summary", None, false, None);
    s.require_output = true;
    let prompt = stage_setup_from(
        &s,
        hints(true),
        Default::default(),
        Some(leviath_core::output::OutputSpec::default()),
    )
    .system_prompt
    .expect("the demand stands on its own");
    assert!(prompt.contains("submit_output"), "{prompt}");
}

// ── Required-output gate ─────────────────────────────────────────────────────

/// A blueprint whose single stage owes a final output.
fn owing_bp(max_revisits: Option<usize>) -> AgentBlueprint {
    let mut stage = stage_named("summary", None, true, max_revisits);
    stage.available_tools = vec![leviath_tools::SUBMIT_OUTPUT_TOOL.to_string()];
    stage.require_output = true;
    let layout = leviath_core::layout::ContextLayout::new(vec![], 10_000);
    AgentBlueprint(leviath_core::Blueprint::new(
        "t".to_string(),
        "d".to_string(),
        vec![stage],
        layout,
    ))
}

fn owing_state() -> AgentState {
    let mut s = agent_state();
    s.current_stage = "summary".to_string();
    s
}

fn conversation_window() -> ContextWindow {
    let mut w = ContextWindow::new(100_000);
    w.add_region(Region::new(
        "conversation".to_string(),
        RegionKind::Clearable,
        10_000,
    ));
    w
}

fn submitted_in(stage: &str) -> crate::persistence::FinalOutput {
    crate::persistence::FinalOutput(leviath_core::output::FinalOutput::new(
        "the answer",
        None,
        stage.to_string(),
        0,
    ))
}

fn run_require_output(world: &mut World) {
    let mut s = Schedule::default();
    s.add_systems(require_final_output);
    s.run(world);
}

#[test]
fn a_stage_that_owes_an_output_and_gave_none_is_nudged_and_re_run() {
    let mut world = World::new();
    let e = world
        .spawn((
            owing_bp(None),
            StageCursor { index: 0 },
            owing_state(),
            conversation_window(),
            ResolveTransition,
        ))
        .id();
    run_require_output(&mut world);
    assert!(world.get::<ReadyToInfer>(e).is_some(), "sent back to work");
    assert!(world.get::<ResolveTransition>(e).is_none(), "not advancing");
    assert_eq!(world.get::<OutputReentries>(e).expect("counted").0, 1);
    assert!(
        world
            .get::<ContextWindow>(e)
            .expect("window")
            .get_region("conversation")
            .expect("region")
            .current_tokens
            > 0,
        "the nudge reached the model"
    );
}

#[test]
fn a_stage_that_submitted_transitions_untouched() {
    let mut world = World::new();
    let e = world
        .spawn((
            owing_bp(None),
            StageCursor { index: 0 },
            owing_state(),
            conversation_window(),
            ResolveTransition,
            submitted_in("summary"),
        ))
        .id();
    run_require_output(&mut world);
    assert!(world.get::<ResolveTransition>(e).is_some());
    assert!(world.get::<OutputReentries>(e).is_none());
}

/// An answer submitted by an earlier stage does not discharge this stage's
/// obligation, or a blueprint whose worker submits would let its summary stage
/// coast on the worker's answer.
#[test]
fn an_output_from_an_earlier_stage_does_not_satisfy_this_one() {
    let mut world = World::new();
    let e = world
        .spawn((
            owing_bp(None),
            StageCursor { index: 0 },
            owing_state(),
            conversation_window(),
            ResolveTransition,
            submitted_in("some_earlier_stage"),
        ))
        .id();
    run_require_output(&mut world);
    assert!(world.get::<ReadyToInfer>(e).is_some(), "still owes its own");
}

#[test]
fn a_stage_that_owes_nothing_is_never_held() {
    let mut world = World::new();
    let mut bp = owing_bp(None);
    bp.0.stages[0].require_output = false;
    let e = world
        .spawn((
            bp,
            StageCursor { index: 0 },
            owing_state(),
            conversation_window(),
            ResolveTransition,
        ))
        .id();
    run_require_output(&mut world);
    assert!(world.get::<ResolveTransition>(e).is_some());
}

/// A missing output never strands a run. When the budget is spent the
/// transition proceeds and the run says so, the way a forced edge gate does.
/// The retry budget is its own, not the stage's `max_revisits`.
///
/// Those are different questions - how many times the graph may re-enter a
/// stage, and how many times a model that owes an answer is nudged - and
/// borrowing the first for the second let a routing setting silently multiply
/// an inference bill. Each retry re-sends the whole stage context, and an
/// output stage runs last, when that context is largest.
#[test]
fn a_generous_max_revisits_does_not_buy_more_output_retries() {
    let mut world = World::new();
    let e = world
        .spawn((
            owing_bp(Some(20)),
            StageCursor { index: 0 },
            owing_state(),
            conversation_window(),
            ResolveTransition,
            OutputReentries(leviath_core::blueprint::DEFAULT_OUTPUT_REENTRY_CAP),
            crate::persistence::RunOutcomeFlags::default(),
        ))
        .id();
    run_require_output(&mut world);
    assert!(
        world.get::<ReadyToInfer>(e).is_none(),
        "max_revisits = 20 must not buy 20 retries of a missing output"
    );
    assert_eq!(
        world
            .get::<crate::persistence::RunOutcomeFlags>(e)
            .expect("flags")
            .0
            .output_forced,
        1
    );
}

#[test]
fn an_exhausted_budget_proceeds_and_records_that_it_was_forced() {
    let mut world = World::new();
    let e = world
        .spawn((
            owing_bp(Some(2)),
            StageCursor { index: 0 },
            owing_state(),
            conversation_window(),
            ResolveTransition,
            OutputReentries(leviath_core::blueprint::DEFAULT_OUTPUT_REENTRY_CAP),
            crate::persistence::RunOutcomeFlags::default(),
        ))
        .id();
    run_require_output(&mut world);
    assert!(
        world.get::<ResolveTransition>(e).is_some(),
        "the run finishes rather than hanging"
    );
    assert_eq!(
        world
            .get::<crate::persistence::RunOutcomeFlags>(e)
            .expect("flags")
            .0
            .output_forced,
        1,
        "and the run explains itself afterwards"
    );
}

/// The flags are optional on the entity, and an agent without them still has to
/// finish. Recording the outcome is a courtesy to whoever reads the run
/// afterwards; it is not what keeps the run moving.
#[test]
fn an_exhausted_budget_proceeds_even_with_nowhere_to_record_it() {
    let mut world = World::new();
    let e = world
        .spawn((
            owing_bp(Some(2)),
            StageCursor { index: 0 },
            owing_state(),
            conversation_window(),
            ResolveTransition,
            OutputReentries(leviath_core::blueprint::DEFAULT_OUTPUT_REENTRY_CAP),
        ))
        .id();
    run_require_output(&mut world);
    assert!(
        world.get::<ResolveTransition>(e).is_some(),
        "the run finishes rather than hanging on a missing component"
    );
}

/// An agent that already failed should follow its error edge, not be told to
/// summarise.
#[test]
fn an_errored_or_capped_stage_is_left_to_its_own_transition() {
    for outcome in [
        StageOutcome::Errored("boom".to_string()),
        StageOutcome::MaxIterations,
    ] {
        let mut world = World::new();
        let e = world
            .spawn((
                owing_bp(None),
                StageCursor { index: 0 },
                owing_state(),
                conversation_window(),
                ResolveTransition,
                outcome,
            ))
            .id();
        run_require_output(&mut world);
        assert!(world.get::<ResolveTransition>(e).is_some());
        assert!(world.get::<OutputReentries>(e).is_none());
    }
}

/// The stage still follows its own transition, but the run has to say the
/// requirement went unmet.
///
/// This is the ordinary way a required output goes missing, not an edge case: a
/// model that cannot satisfy its validator retries until its iterations run out
/// and leaves on the max-iterations path. Left unrecorded the run reports
/// `output_forced: 0`, which reads as "nothing was required" rather than "the
/// requirement went unmet", and a fan-out parent counts that worker as a
/// success.
#[test]
fn an_errored_or_capped_stage_still_records_the_missing_output() {
    for outcome in [
        StageOutcome::Errored("boom".to_string()),
        StageOutcome::MaxIterations,
    ] {
        let mut world = World::new();
        let e = world
            .spawn((
                owing_bp(None),
                StageCursor { index: 0 },
                owing_state(),
                conversation_window(),
                ResolveTransition,
                outcome.clone(),
                crate::persistence::RunOutcomeFlags::default(),
            ))
            .id();
        run_require_output(&mut world);
        assert_eq!(
            world
                .get::<crate::persistence::RunOutcomeFlags>(e)
                .expect("flags")
                .0
                .output_forced,
            1,
            "{outcome:?} left the stage owing an output and the run must say so"
        );
    }
}

/// A stage that owes nothing is not flagged just because it errored, or every
/// failed run would claim a missing output it never promised.
#[test]
fn an_errored_stage_that_owes_nothing_is_not_flagged() {
    let mut world = World::new();
    let mut bp = owing_bp(None);
    bp.0.stages[0].require_output = false;
    let e = world
        .spawn((
            bp,
            StageCursor { index: 0 },
            owing_state(),
            conversation_window(),
            ResolveTransition,
            StageOutcome::MaxIterations,
            crate::persistence::RunOutcomeFlags::default(),
        ))
        .id();
    run_require_output(&mut world);
    assert_eq!(
        world
            .get::<crate::persistence::RunOutcomeFlags>(e)
            .expect("flags")
            .0
            .output_forced,
        0
    );
}

/// Entering a new stage re-arms the gate: each stage owes its own output and
/// gets its own budget of attempts.
#[test]
fn entering_a_stage_clears_the_output_reentry_count() {
    let mut world = World::new();
    let bp = owing_bp(None);
    let setup = stage_setup_from(&bp.0.stages[0], hints(true), Default::default(), None);
    let e = world.spawn((OutputReentries(3),)).id();
    let inf = StageInference {
        provider_name: "p".to_string(),
        model: "m".to_string(),
        tools: vec![],
        tool_filter: None,
        fallbacks: vec![],
        output: None,
    };
    {
        let mut commands = world.commands();
        attach_stage_components(commands.entity(e), inf, &setup, 0, "summary".to_string());
    }
    world.flush();
    assert!(world.get::<OutputReentries>(e).is_none());
}

// ─── on_stage_enter (issue #260) ─────────────────────────────────────────────

fn hook_scripts(src: &str, wanted: &[&str]) -> crate::components::StageHookScripts {
    let compiled = leviath_scripting::stage_hook::compile("h.rhai", src, wanted)
        .expect("the fixture script compiles");
    let mut map = std::collections::HashMap::new();
    map.insert("h.rhai".to_string(), std::sync::Arc::new(compiled));
    crate::components::StageHookScripts(map)
}

/// A one-stage blueprint whose stage names `h.rhai` for `on_stage_enter`.
fn hooked_bp() -> AgentBlueprint {
    let mut stage = leviath_core::Stage::new(
        "main".to_string(),
        leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
    );
    stage.hooks.on_stage_enter = Some("h.rhai".to_string());
    AgentBlueprint(blueprint(vec![stage]))
}

fn spawn_hooked(world: &mut World, src: &str) -> Entity {
    world
        .spawn((
            hooked_bp(),
            agent_state(),
            conv_window(),
            StageJustEntered {
                index: 0,
                name: "main".to_string(),
            },
            hook_scripts(src, &["on_stage_enter"]),
        ))
        .id()
}

fn run_stage_hooks(world: &mut World) {
    let mut schedule = Schedule::default();
    schedule.add_systems(run_stage_enter_hooks);
    schedule.run(world);
}

fn region_text(world: &World, e: Entity, name: &str) -> String {
    world
        .get::<ContextWindow>(e)
        .expect("window")
        .get_region(name)
        .expect("region")
        .content
        .iter()
        .map(|x| x.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_hook_that_allows_changes_nothing_and_leaves_the_agent_running() {
    let mut world = World::new();
    let e = spawn_hooked(&mut world, "fn on_stage_enter(ctx) { () }");
    run_stage_hooks(&mut world);
    assert_eq!(region_text(&world, e, "conversation"), "");
    assert!(matches!(
        world.get::<AgentState>(e).expect("state").status,
        AgentStatus::Active | AgentStatus::Idle
    ));
}

/// The hook's whole point: seed a region before the stage's first inference.
#[test]
fn a_hook_can_write_a_region_on_entry() {
    let mut world = World::new();
    let e = spawn_hooked(
        &mut world,
        r#"fn on_stage_enter(ctx) { #{ action: "modify", value: #{ conversation: "seeded" } } }"#,
    );
    run_stage_hooks(&mut world);
    assert_eq!(region_text(&world, e, "conversation"), "seeded");
}

/// The script is shown the stage it is entering, not a placeholder.
#[test]
fn the_hook_sees_the_stage_it_is_entering() {
    let mut world = World::new();
    let e = spawn_hooked(
        &mut world,
        r#"fn on_stage_enter(ctx) { #{ action: "modify", value: #{ conversation: ctx.stage } } }"#,
    );
    run_stage_hooks(&mut world);
    assert_eq!(region_text(&world, e, "conversation"), "main");
}

/// Replace, not append - otherwise a hook that echoes its input doubles the
/// region every time the stage is re-entered.
#[test]
fn writing_a_region_replaces_it_rather_than_appending() {
    let mut world = World::new();
    let e = spawn_hooked(
        &mut world,
        r#"fn on_stage_enter(ctx) { #{ action: "modify", value: #{ conversation: "once" } } }"#,
    );
    run_stage_hooks(&mut world);
    // Re-enter the same stage; the marker is what drives the hook.
    world.entity_mut(e).insert(StageJustEntered {
        index: 0,
        name: "main".to_string(),
    });
    run_stage_hooks(&mut world);
    assert_eq!(region_text(&world, e, "conversation"), "once");
}

#[test]
fn a_hook_can_refuse_the_stage_and_the_reason_reaches_the_run() {
    let mut world = World::new();
    let e = spawn_hooked(
        &mut world,
        r#"fn on_stage_enter(ctx) { #{ action: "cancel", reason: "over budget" } }"#,
    );
    run_stage_hooks(&mut world);
    let AgentStatus::Error { message } = &world.get::<AgentState>(e).expect("state").status else {
        panic!("a refused stage must error the run");
    };
    assert!(message.contains("over budget"), "{message}");
}

/// A hook that throws is not a hook that allowed. Treating a failed script as
/// permission is how a gate silently stops gating.
#[test]
fn a_hook_that_fails_errors_the_run_rather_than_proceeding() {
    let mut world = World::new();
    let e = spawn_hooked(&mut world, r#"fn on_stage_enter(ctx) { throw "boom" }"#);
    run_stage_hooks(&mut world);
    let AgentStatus::Error { message } = &world.get::<AgentState>(e).expect("state").status else {
        panic!("a failing hook must error the run");
    };
    assert!(message.contains("on_stage_enter hook failed"), "{message}");
}

#[test]
fn writing_a_region_the_stage_does_not_have_errors_rather_than_being_dropped() {
    let mut world = World::new();
    let e = spawn_hooked(
        &mut world,
        r#"fn on_stage_enter(ctx) { #{ action: "modify", value: #{ nope: "x" } } }"#,
    );
    run_stage_hooks(&mut world);
    let AgentStatus::Error { message } = &world.get::<AgentState>(e).expect("state").status else {
        panic!("writing an unknown region must error");
    };
    assert!(message.contains("no region 'nope'"), "{message}");
}

#[test]
fn a_non_string_region_value_errors() {
    let mut world = World::new();
    let e = spawn_hooked(
        &mut world,
        r#"fn on_stage_enter(ctx) { #{ action: "modify", value: #{ conversation: 42 } } }"#,
    );
    run_stage_hooks(&mut world);
    assert!(matches!(
        world.get::<AgentState>(e).expect("state").status,
        AgentStatus::Error { .. }
    ));
}

#[test]
fn a_modify_value_that_is_not_a_map_errors() {
    let mut world = World::new();
    let e = spawn_hooked(
        &mut world,
        r#"fn on_stage_enter(ctx) { #{ action: "modify", value: "just a string" } }"#,
    );
    run_stage_hooks(&mut world);
    assert!(matches!(
        world.get::<AgentState>(e).expect("state").status,
        AgentStatus::Error { .. }
    ));
}

/// `retry` has no meaning for a stage already entered. Saying so beats treating
/// it as allow, which would let a script believe it had asked for something.
#[test]
fn retry_is_reported_as_unhonourable_rather_than_silently_allowed() {
    let mut world = World::new();
    let e = spawn_hooked(
        &mut world,
        r#"fn on_stage_enter(ctx) { #{ action: "retry" } }"#,
    );
    run_stage_hooks(&mut world);
    let AgentStatus::Error { message } = &world.get::<AgentState>(e).expect("state").status else {
        panic!("an unhonourable outcome must be reported");
    };
    assert!(message.contains("cannot honour"), "{message}");
}

/// An agent with no hook component is skipped entirely - this is what "no
/// hooks, no cost" means, and the system must not panic on its absence.
#[test]
fn an_agent_without_hooks_is_untouched() {
    let mut world = World::new();
    let e = world
        .spawn((
            hooked_bp(),
            agent_state(),
            conv_window(),
            StageJustEntered {
                index: 0,
                name: "main".to_string(),
            },
        ))
        .id();
    run_stage_hooks(&mut world);
    assert_eq!(region_text(&world, e, "conversation"), "");
}

/// A stage index the blueprint does not have cannot be looked up; the system
/// skips rather than panicking on the slice.
#[test]
fn an_out_of_range_stage_index_is_skipped() {
    let mut world = World::new();
    let e = world
        .spawn((
            hooked_bp(),
            agent_state(),
            conv_window(),
            StageJustEntered {
                index: 99,
                name: "gone".to_string(),
            },
            hook_scripts(
                r#"fn on_stage_enter(ctx) { #{ action: "cancel", reason: "should not run" } }"#,
                &["on_stage_enter"],
            ),
        ))
        .id();
    run_stage_hooks(&mut world);
    assert!(!matches!(
        world.get::<AgentState>(e).expect("state").status,
        AgentStatus::Error { .. }
    ));
}

/// A stage that declares no hook is not run even when the agent carries
/// scripts - another stage in the same blueprint may have declared them.
#[test]
fn a_stage_that_declares_no_hook_does_not_run_one() {
    let mut world = World::new();
    let stage = leviath_core::Stage::new(
        "main".to_string(),
        leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
    );
    let e = world
        .spawn((
            AgentBlueprint(blueprint(vec![stage])),
            agent_state(),
            conv_window(),
            StageJustEntered {
                index: 0,
                name: "main".to_string(),
            },
            hook_scripts(
                r#"fn on_stage_enter(ctx) { #{ action: "modify", value: #{ conversation: "ran" } } }"#,
                &["on_stage_enter"],
            ),
        ))
        .id();
    run_stage_hooks(&mut world);
    assert_eq!(region_text(&world, e, "conversation"), "");
}

/// A write the region cannot hold is reported, not swallowed. `add_entry`
/// refuses an entry over the region's budget, and a hook whose write silently
/// vanished would look exactly like one that chose to write nothing.
#[test]
fn a_region_write_that_does_not_fit_errors() {
    let mut world = World::new();
    let mut window = ContextWindow::new(10_000);
    // A budget too small for anything the hook could write.
    window.add_region(Region::new(
        "conversation".to_string(),
        RegionKind::Clearable,
        1,
    ));
    let e = world
        .spawn((
            hooked_bp(),
            agent_state(),
            window,
            StageJustEntered {
                index: 0,
                name: "main".to_string(),
            },
            hook_scripts(
                r#"fn on_stage_enter(ctx) { #{ action: "modify", value: #{ conversation: "a much longer string than one token" } } }"#,
                &["on_stage_enter"],
            ),
        ))
        .id();
    run_stage_hooks(&mut world);
    let AgentStatus::Error { message } = &world.get::<AgentState>(e).expect("state").status else {
        panic!("a write that does not fit must error");
    };
    assert!(
        message.contains("writing region 'conversation'"),
        "{message}"
    );
}

/// Writing an empty string clears the region rather than storing a blank
/// entry - "" is how a hook says "there should be nothing here".
#[test]
fn writing_an_empty_string_clears_the_region() {
    let mut world = World::new();
    let mut window = conv_window();
    window
        .get_region_mut("conversation")
        .expect("region")
        .add_entry("something".to_string(), 1)
        .expect("seeded");
    let e = world
        .spawn((
            hooked_bp(),
            agent_state(),
            window,
            StageJustEntered {
                index: 0,
                name: "main".to_string(),
            },
            hook_scripts(
                r#"fn on_stage_enter(ctx) { #{ action: "modify", value: #{ conversation: "" } } }"#,
                &["on_stage_enter"],
            ),
        ))
        .id();
    run_stage_hooks(&mut world);
    assert_eq!(region_text(&world, e, "conversation"), "");
    assert!(
        world
            .get::<ContextWindow>(e)
            .expect("window")
            .get_region("conversation")
            .expect("region")
            .content
            .is_empty(),
        "an empty write leaves no entry behind"
    );
}

/// A bare `false` refuses with no reason. The message still has to say the
/// stage was refused, or an operator sees a failed run and no cause at all.
#[test]
fn a_refusal_without_a_reason_still_says_it_was_refused() {
    let mut world = World::new();
    let e = spawn_hooked(&mut world, "fn on_stage_enter(ctx) { false }");
    run_stage_hooks(&mut world);
    let AgentStatus::Error { message } = &world.get::<AgentState>(e).expect("state").status else {
        panic!("a bare false must refuse");
    };
    assert!(message.contains("refused stage 'main'"), "{message}");
    assert!(message.contains("no reason given"), "{message}");
}

// ─── before_inference / after_inference (issue #260) ─────────────────────────

fn stage_hooked(
    field: impl FnOnce(&mut leviath_core::blueprint::StageHooks, String),
) -> AgentBlueprint {
    let mut stage = leviath_core::Stage::new(
        "main".to_string(),
        leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
    );
    field(&mut stage.hooks, "h.rhai".to_string());
    AgentBlueprint(blueprint(vec![stage]))
}

fn run_before_hooks(world: &mut World) {
    let mut schedule = Schedule::default();
    schedule.add_systems(run_before_inference_hooks);
    schedule.run(world);
}

fn run_after_hooks(world: &mut World) {
    let mut schedule = Schedule::default();
    schedule.add_systems(run_after_inference_hooks);
    schedule.run(world);
}

fn spawn_before(world: &mut World, src: &str) -> Entity {
    world
        .spawn((
            stage_hooked(|h, p| h.before_inference = Some(p)),
            agent_state(),
            conv_window(),
            StageCursor { index: 0 },
            ReadyToInfer,
            hook_scripts(src, &["before_inference"]),
        ))
        .id()
}

fn spawn_after(world: &mut World, src: &str) -> Entity {
    world
        .spawn((
            stage_hooked(|h, p| h.after_inference = Some(p)),
            agent_state(),
            StageCursor { index: 0 },
            ProcessResponse,
            crate::components::InferenceResult {
                response: "the raw answer".to_string(),
                tool_calls: vec![],
                tokens_used: 7,
                timestamp: 0,
            },
            hook_scripts(src, &["after_inference"]),
        ))
        .id()
}

fn status_message(world: &World, e: Entity) -> Option<String> {
    match &world.get::<AgentState>(e).expect("state").status {
        AgentStatus::Error { message } => Some(message.clone()),
        _ => None,
    }
}

#[test]
fn before_inference_can_seed_the_window_the_request_is_built_from() {
    let mut world = World::new();
    let e = spawn_before(
        &mut world,
        r#"fn before_inference(ctx) { #{ action: "modify", value: #{ conversation: "injected" } } }"#,
    );
    run_before_hooks(&mut world);
    assert_eq!(region_text(&world, e, "conversation"), "injected");
    assert!(status_message(&world, e).is_none());
}

/// A refused inference stops the agent *and* takes back `ReadyToInfer`, so
/// `dispatch_inference` cannot pick it up in the same tick - refusing while
/// still letting the call go is not refusing.
#[test]
fn before_inference_can_refuse_the_call_and_the_agent_stops_being_ready() {
    let mut world = World::new();
    let e = spawn_before(
        &mut world,
        r#"fn before_inference(ctx) { #{ action: "cancel", reason: "over budget" } }"#,
    );
    run_before_hooks(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("over budget")
    );
    assert!(
        world.get::<ReadyToInfer>(e).is_none(),
        "a refused inference must not stay dispatchable"
    );
}

#[test]
fn before_inference_that_throws_errors_the_run() {
    let mut world = World::new();
    let e = spawn_before(&mut world, r#"fn before_inference(ctx) { throw "no" }"#);
    run_before_hooks(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("hook failed")
    );
}

#[test]
fn before_inference_retry_is_refused_as_unhonourable() {
    let mut world = World::new();
    let e = spawn_before(
        &mut world,
        r#"fn before_inference(ctx) { #{ action: "retry" } }"#,
    );
    run_before_hooks(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("cannot honour")
    );
}

#[test]
fn before_inference_bad_modify_errors() {
    let mut world = World::new();
    let e = spawn_before(
        &mut world,
        r#"fn before_inference(ctx) { #{ action: "modify", value: #{ nope: "x" } } }"#,
    );
    run_before_hooks(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("no region 'nope'")
    );
}

#[test]
fn before_inference_allow_changes_nothing() {
    let mut world = World::new();
    let e = spawn_before(&mut world, "fn before_inference(ctx) { () }");
    run_before_hooks(&mut world);
    assert_eq!(region_text(&world, e, "conversation"), "");
    assert!(status_message(&world, e).is_none());
    assert!(world.get::<ReadyToInfer>(e).is_some());
}

#[test]
fn before_inference_skips_an_out_of_range_stage() {
    let mut world = World::new();
    let e = world
        .spawn((
            stage_hooked(|h, p| h.before_inference = Some(p)),
            agent_state(),
            conv_window(),
            StageCursor { index: 99 },
            ReadyToInfer,
            hook_scripts(
                r#"fn before_inference(ctx) { #{ action: "cancel" } }"#,
                &["before_inference"],
            ),
        ))
        .id();
    run_before_hooks(&mut world);
    assert!(status_message(&world, e).is_none());
}

#[test]
fn before_inference_skips_a_stage_that_declared_none() {
    let mut world = World::new();
    let stage = leviath_core::Stage::new(
        "main".to_string(),
        leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
    );
    let e = world
        .spawn((
            AgentBlueprint(blueprint(vec![stage])),
            agent_state(),
            conv_window(),
            StageCursor { index: 0 },
            ReadyToInfer,
            hook_scripts(
                r#"fn before_inference(ctx) { #{ action: "cancel" } }"#,
                &["before_inference"],
            ),
        ))
        .id();
    run_before_hooks(&mut world);
    assert!(status_message(&world, e).is_none());
}

// ── after_inference ──

#[test]
fn after_inference_can_rewrite_the_response() {
    let mut world = World::new();
    let e = spawn_after(
        &mut world,
        r#"fn after_inference(ctx) { #{ action: "modify", value: "cleaned up" } }"#,
    );
    run_after_hooks(&mut world);
    assert_eq!(
        world
            .get::<crate::components::InferenceResult>(e)
            .expect("result")
            .response,
        "cleaned up"
    );
}

/// The hook is shown the real response, not a placeholder.
#[test]
fn after_inference_sees_the_response_and_its_token_count() {
    let mut world = World::new();
    let e = spawn_after(
        &mut world,
        r#"fn after_inference(ctx) { #{ action: "modify", value: ctx.response + "/" + ctx.tokens_used } }"#,
    );
    run_after_hooks(&mut world);
    assert_eq!(
        world
            .get::<crate::components::InferenceResult>(e)
            .expect("result")
            .response,
        "the raw answer/7"
    );
}

#[test]
fn after_inference_can_reject_the_response() {
    let mut world = World::new();
    let e = spawn_after(
        &mut world,
        r#"fn after_inference(ctx) { #{ action: "cancel", reason: "not valid json" } }"#,
    );
    run_after_hooks(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("not valid json")
    );
}

#[test]
fn after_inference_modify_must_be_text() {
    let mut world = World::new();
    let e = spawn_after(
        &mut world,
        r#"fn after_inference(ctx) { #{ action: "modify", value: #{ not: "text" } } }"#,
    );
    run_after_hooks(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("replacement response text")
    );
}

/// Re-inference needs an attempt bound before it can be offered, or a hook that
/// always retries wedges the run. Refused explicitly rather than ignored.
#[test]
fn after_inference_retry_is_refused_with_the_reason_it_is_not_implemented() {
    let mut world = World::new();
    let e = spawn_after(
        &mut world,
        r#"fn after_inference(ctx) { #{ action: "retry" } }"#,
    );
    run_after_hooks(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("not implemented yet")
    );
}

#[test]
fn after_inference_that_throws_errors_the_run() {
    let mut world = World::new();
    let e = spawn_after(&mut world, r#"fn after_inference(ctx) { throw "no" }"#);
    run_after_hooks(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("hook failed")
    );
}

#[test]
fn after_inference_allow_leaves_the_response_alone() {
    let mut world = World::new();
    let e = spawn_after(&mut world, "fn after_inference(ctx) { () }");
    run_after_hooks(&mut world);
    assert_eq!(
        world
            .get::<crate::components::InferenceResult>(e)
            .expect("result")
            .response,
        "the raw answer"
    );
    assert!(status_message(&world, e).is_none());
}

/// Tool calls reach the hook as names only. It can notice what the model wants
/// to run; it cannot rewrite the call, because the policy and taint layers are
/// about to check exactly those and a hook that could edit them would be a way
/// around checks the operator configured.
#[test]
fn after_inference_sees_tool_call_names_but_cannot_change_them() {
    let mut world = World::new();
    let e = world
        .spawn((
            stage_hooked(|h, p| h.after_inference = Some(p)),
            agent_state(),
            StageCursor { index: 0 },
            ProcessResponse,
            crate::components::InferenceResult {
                response: String::new(),
                tool_calls: vec![crate::components::ToolCall {
                    tool_id: "c1".to_string(),
                    name: "shell".to_string(),
                    arguments: serde_json::json!({"command": "ls"}),
                    thought_signature: None,
                }],
                tokens_used: 0,
                timestamp: 0,
            },
            hook_scripts(
                r#"fn after_inference(ctx) { #{ action: "modify", value: ctx.tool_calls[0] } }"#,
                &["after_inference"],
            ),
        ))
        .id();
    run_after_hooks(&mut world);
    let result = world
        .get::<crate::components::InferenceResult>(e)
        .expect("result");
    assert_eq!(result.response, "shell", "the hook saw the call's name");
    assert_eq!(
        result.tool_calls.len(),
        1,
        "and the call itself is untouched"
    );
    assert_eq!(result.tool_calls[0].name, "shell");
    assert_eq!(result.tool_calls[0].arguments["command"], "ls");
}

#[test]
fn after_inference_skips_an_out_of_range_stage() {
    let mut world = World::new();
    let e = world
        .spawn((
            stage_hooked(|h, p| h.after_inference = Some(p)),
            agent_state(),
            StageCursor { index: 99 },
            ProcessResponse,
            crate::components::InferenceResult {
                response: "x".to_string(),
                tool_calls: vec![],
                tokens_used: 0,
                timestamp: 0,
            },
            hook_scripts(
                r#"fn after_inference(ctx) { #{ action: "cancel" } }"#,
                &["after_inference"],
            ),
        ))
        .id();
    run_after_hooks(&mut world);
    assert!(status_message(&world, e).is_none());
}

#[test]
fn after_inference_skips_a_stage_that_declared_none() {
    let mut world = World::new();
    let stage = leviath_core::Stage::new(
        "main".to_string(),
        leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
    );
    let e = world
        .spawn((
            AgentBlueprint(blueprint(vec![stage])),
            agent_state(),
            StageCursor { index: 0 },
            ProcessResponse,
            crate::components::InferenceResult {
                response: "x".to_string(),
                tool_calls: vec![],
                tokens_used: 0,
                timestamp: 0,
            },
            hook_scripts(
                r#"fn after_inference(ctx) { #{ action: "cancel" } }"#,
                &["after_inference"],
            ),
        ))
        .id();
    run_after_hooks(&mut world);
    assert!(status_message(&world, e).is_none());
}

/// A bare `false` from either inference hook refuses with no reason given, and
/// the message still has to say what was refused - an operator seeing a failed
/// run needs the cause, not just the failure.
#[test]
fn an_inference_hook_refusing_without_a_reason_still_says_what_it_refused() {
    let mut world = World::new();
    let before = spawn_before(&mut world, "fn before_inference(ctx) { false }");
    run_before_hooks(&mut world);
    let msg = status_message(&world, before).expect("errored");
    assert!(msg.contains("refused the inference"), "{msg}");
    assert!(msg.contains("no reason given"), "{msg}");

    let mut world = World::new();
    let after = spawn_after(&mut world, "fn after_inference(ctx) { false }");
    run_after_hooks(&mut world);
    let msg = status_message(&world, after).expect("errored");
    assert!(msg.contains("rejected the response"), "{msg}");
    assert!(msg.contains("no reason given"), "{msg}");
}

// ─── on_tool_call (issue #260) ───────────────────────────────────────────────

fn call(name: &str, args: serde_json::Value) -> crate::components::ToolCall {
    crate::components::ToolCall {
        tool_id: format!("c-{name}"),
        name: name.to_string(),
        arguments: args,
        thought_signature: Some("provider-token".to_string()),
    }
}

fn spawn_tool_hooked(
    world: &mut World,
    src: &str,
    calls: Vec<crate::components::ToolCall>,
) -> Entity {
    world
        .spawn((
            stage_hooked(|h, p| h.on_tool_call = Some(p)),
            agent_state(),
            StageCursor { index: 0 },
            ReadyForTools,
            crate::components::InferenceResult {
                response: String::new(),
                tool_calls: calls,
                tokens_used: 0,
                timestamp: 0,
            },
            hook_scripts(src, &["on_tool_call"]),
        ))
        .id()
}

fn run_tool_hooks(world: &mut World) {
    let mut schedule = Schedule::default();
    schedule.add_systems(run_tool_call_hooks);
    schedule.run(world);
}

fn calls_of(world: &World, e: Entity) -> Vec<crate::components::ToolCall> {
    world
        .get::<crate::components::InferenceResult>(e)
        .expect("result")
        .tool_calls
        .clone()
}

#[test]
fn on_tool_call_sees_the_calls_and_their_arguments() {
    let mut world = World::new();
    let e = spawn_tool_hooked(
        &mut world,
        r#"fn on_tool_call(ctx) {
             #{ action: "modify",
                value: [#{ name: ctx.tool_calls[0].name + "-seen",
                           arguments: ctx.tool_calls[0].arguments }] }
           }"#,
        vec![call("shell", serde_json::json!({"command": "ls"}))],
    );
    run_tool_hooks(&mut world);
    let got = calls_of(&world, e);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].name, "shell-seen");
    assert_eq!(got[0].arguments["command"], "ls");
}

/// Narrowing is the point: a hook can rewrite a call into something tamer.
#[test]
fn on_tool_call_can_rewrite_arguments() {
    let mut world = World::new();
    let e = spawn_tool_hooked(
        &mut world,
        r#"fn on_tool_call(ctx) {
             #{ action: "modify",
                value: [#{ name: "shell", arguments: #{ command: "ls -la" } }] }
           }"#,
        vec![call("shell", serde_json::json!({"command": "rm -rf /"}))],
    );
    run_tool_hooks(&mut world);
    assert_eq!(calls_of(&world, e)[0].arguments["command"], "ls -la");
}

#[test]
fn on_tool_call_can_drop_calls_entirely() {
    let mut world = World::new();
    let e = spawn_tool_hooked(
        &mut world,
        r#"fn on_tool_call(ctx) { #{ action: "modify", value: [] } }"#,
        vec![call("shell", serde_json::json!({}))],
    );
    run_tool_hooks(&mut world);
    assert!(calls_of(&world, e).is_empty());
}

/// **The safety property.** A hook has no access to the taint gate, the
/// auto-approve marker, or tool sensitivities - it edits the calls and nothing
/// else, so whatever it produces still faces the policy layer. If this ever
/// stops holding, a hook becomes a way around the operator's configuration.
#[test]
fn on_tool_call_cannot_mark_its_own_calls_approved() {
    let mut world = World::new();
    let e = world
        .spawn((
            stage_hooked(|h, p| h.on_tool_call = Some(p)),
            agent_state(),
            StageCursor { index: 0 },
            ReadyForTools,
            crate::components::InferenceResult {
                response: String::new(),
                tool_calls: vec![call("shell", serde_json::json!({"command": "ls"}))],
                tokens_used: 0,
                timestamp: 0,
            },
            crate::taint::TaintGate::new(leviath_core::taint::SecurityConfig::default()),
            hook_scripts(
                r#"fn on_tool_call(ctx) {
                     #{ action: "modify",
                        value: [#{ name: "shell", arguments: #{ command: "anything" } }] }
                   }"#,
                &["on_tool_call"],
            ),
        ))
        .id();
    let before = format!("{:?}", world.get::<crate::taint::TaintGate>(e));

    run_tool_hooks(&mut world);

    // The call was rewritten...
    assert_eq!(calls_of(&world, e)[0].arguments["command"], "anything");
    // ...and nothing about the gate moved, so the policy layer still decides.
    assert_eq!(
        format!("{:?}", world.get::<crate::taint::TaintGate>(e)),
        before,
        "a hook must not be able to pre-approve its own calls"
    );
    assert!(
        world.get::<crate::components::GateAutoApprove>(e).is_none(),
        "a hook must not be able to set auto-approve"
    );
}

/// A rewritten call drops the provider's thought signature: that token
/// describes the call the *model* produced, and echoing it back with different
/// arguments would attribute the hook's call to the model.
#[test]
fn a_rewritten_call_does_not_carry_the_models_thought_signature() {
    let mut world = World::new();
    let e = spawn_tool_hooked(
        &mut world,
        r#"fn on_tool_call(ctx) { #{ action: "modify", value: [#{ name: "shell", arguments: #{} }] } }"#,
        vec![call("shell", serde_json::json!({}))],
    );
    run_tool_hooks(&mut world);
    assert!(calls_of(&world, e)[0].thought_signature.is_none());
}

#[test]
fn on_tool_call_can_veto_with_a_reason() {
    let mut world = World::new();
    let e = spawn_tool_hooked(
        &mut world,
        r#"fn on_tool_call(ctx) { #{ action: "cancel", reason: "not on this stage" } }"#,
        vec![call("shell", serde_json::json!({}))],
    );
    run_tool_hooks(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("not on this stage")
    );
}

#[test]
fn on_tool_call_veto_without_a_reason_still_says_what_it_refused() {
    let mut world = World::new();
    let e = spawn_tool_hooked(
        &mut world,
        "fn on_tool_call(ctx) { false }",
        vec![call("shell", serde_json::json!({}))],
    );
    run_tool_hooks(&mut world);
    let msg = status_message(&world, e).expect("errored");
    assert!(msg.contains("refused the tool calls"), "{msg}");
    assert!(msg.contains("no reason given"), "{msg}");
}

#[test]
fn on_tool_call_allow_leaves_the_calls_alone() {
    let mut world = World::new();
    let e = spawn_tool_hooked(
        &mut world,
        "fn on_tool_call(ctx) { () }",
        vec![call("shell", serde_json::json!({"command": "ls"}))],
    );
    run_tool_hooks(&mut world);
    let got = calls_of(&world, e);
    assert_eq!(got[0].name, "shell");
    assert_eq!(got[0].arguments["command"], "ls");
    assert_eq!(
        got[0].thought_signature.as_deref(),
        Some("provider-token"),
        "an untouched call keeps the provider's token"
    );
}

#[test]
fn on_tool_call_retry_is_refused_as_unhonourable() {
    let mut world = World::new();
    let e = spawn_tool_hooked(
        &mut world,
        r#"fn on_tool_call(ctx) { #{ action: "retry" } }"#,
        vec![call("shell", serde_json::json!({}))],
    );
    run_tool_hooks(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("cannot honour")
    );
}

#[test]
fn on_tool_call_that_throws_errors_the_run() {
    let mut world = World::new();
    let e = spawn_tool_hooked(
        &mut world,
        r#"fn on_tool_call(ctx) { throw "no" }"#,
        vec![call("shell", serde_json::json!({}))],
    );
    run_tool_hooks(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("hook failed")
    );
}

#[test]
fn a_replacement_that_is_not_an_array_errors() {
    let mut world = World::new();
    let e = spawn_tool_hooked(
        &mut world,
        r#"fn on_tool_call(ctx) { #{ action: "modify", value: "nope" } }"#,
        vec![call("shell", serde_json::json!({}))],
    );
    run_tool_hooks(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("must be an array")
    );
}

#[test]
fn a_replacement_call_without_a_name_errors() {
    let mut world = World::new();
    let e = spawn_tool_hooked(
        &mut world,
        r#"fn on_tool_call(ctx) { #{ action: "modify", value: [#{ arguments: #{} }] } }"#,
        vec![call("shell", serde_json::json!({}))],
    );
    run_tool_hooks(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("no 'name'")
    );
}

/// A replacement with no `arguments` is a call with none, not an error - some
/// tools take nothing.
#[test]
fn a_replacement_call_without_arguments_is_allowed() {
    let mut world = World::new();
    let e = spawn_tool_hooked(
        &mut world,
        r#"fn on_tool_call(ctx) { #{ action: "modify", value: [#{ name: "list_dir" }] } }"#,
        vec![call("shell", serde_json::json!({}))],
    );
    run_tool_hooks(&mut world);
    assert!(status_message(&world, e).is_none());
    assert_eq!(calls_of(&world, e)[0].name, "list_dir");
}

/// Nothing to inspect means nothing to ask about - the hook is not run at all
/// on a batch with no calls.
#[test]
fn on_tool_call_is_not_run_when_there_are_no_calls() {
    let mut world = World::new();
    let e = spawn_tool_hooked(
        &mut world,
        r#"fn on_tool_call(ctx) { #{ action: "cancel", reason: "should not run" } }"#,
        vec![],
    );
    run_tool_hooks(&mut world);
    assert!(status_message(&world, e).is_none());
}

#[test]
fn on_tool_call_skips_an_out_of_range_stage_and_a_stage_that_declared_none() {
    let mut world = World::new();
    let out_of_range = world
        .spawn((
            stage_hooked(|h, p| h.on_tool_call = Some(p)),
            agent_state(),
            StageCursor { index: 99 },
            ReadyForTools,
            crate::components::InferenceResult {
                response: String::new(),
                tool_calls: vec![call("shell", serde_json::json!({}))],
                tokens_used: 0,
                timestamp: 0,
            },
            hook_scripts(
                r#"fn on_tool_call(ctx) { #{ action: "cancel" } }"#,
                &["on_tool_call"],
            ),
        ))
        .id();
    let stage = leviath_core::Stage::new(
        "main".to_string(),
        leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
    );
    let undeclared = world
        .spawn((
            AgentBlueprint(blueprint(vec![stage])),
            agent_state(),
            StageCursor { index: 0 },
            ReadyForTools,
            crate::components::InferenceResult {
                response: String::new(),
                tool_calls: vec![call("shell", serde_json::json!({}))],
                tokens_used: 0,
                timestamp: 0,
            },
            hook_scripts(
                r#"fn on_tool_call(ctx) { #{ action: "cancel" } }"#,
                &["on_tool_call"],
            ),
        ))
        .id();
    run_tool_hooks(&mut world);
    assert!(status_message(&world, out_of_range).is_none());
    assert!(status_message(&world, undeclared).is_none());
}

// ─── on_completion / on_error (issue #260) ───────────────────────────────────

fn run_terminal(world: &mut World) {
    let mut schedule = Schedule::default();
    schedule.add_systems(run_terminal_hooks);
    schedule.run(world);
}

fn spawn_terminal(
    world: &mut World,
    src: &str,
    hook: &'static str,
    status: AgentStatus,
    answer: Option<&str>,
) -> Entity {
    let mut state = agent_state();
    state.status = status;
    let bp = stage_hooked(move |h, p| match hook {
        "on_completion" => h.on_completion = Some(p),
        _ => h.on_error = Some(p),
    });
    let mut e = world.spawn((
        bp,
        state,
        StageCursor { index: 0 },
        hook_scripts(src, &[hook]),
    ));
    if let Some(a) = answer {
        e.insert(crate::persistence::FinalOutput(
            leviath_core::output::FinalOutput::new(a, None, "s".to_string(), 10),
        ));
    }
    e.id()
}

fn answer_of(world: &World, e: Entity) -> String {
    world
        .get::<crate::persistence::FinalOutput>(e)
        .expect("output")
        .0
        .content
        .clone()
}

#[test]
fn on_completion_can_rewrite_the_answer() {
    let mut world = World::new();
    let e = spawn_terminal(
        &mut world,
        r#"fn on_completion(ctx) { #{ action: "modify", value: "tidied: " + ctx.output } }"#,
        "on_completion",
        AgentStatus::Complete,
        Some("raw answer"),
    );
    run_terminal(&mut world);
    assert_eq!(answer_of(&world, e), "tidied: raw answer");
}

#[test]
fn on_error_can_rewrite_the_message() {
    let mut world = World::new();
    let e = spawn_terminal(
        &mut world,
        r#"fn on_error(ctx) { #{ action: "modify", value: "friendly: " + ctx.error } }"#,
        "on_error",
        AgentStatus::Error {
            message: "raw failure".to_string(),
        },
        None,
    );
    run_terminal(&mut world);
    assert_eq!(
        status_message(&world, e).expect("errored"),
        "friendly: raw failure"
    );
}

/// A terminal status stays true every tick, so without the fire-once marker the
/// hook would run forever - and a rewriting hook would compound its own output.
#[test]
fn a_terminal_hook_runs_exactly_once() {
    let mut world = World::new();
    let e = spawn_terminal(
        &mut world,
        r#"fn on_completion(ctx) { #{ action: "modify", value: ctx.output + "!" } }"#,
        "on_completion",
        AgentStatus::Complete,
        Some("x"),
    );
    run_terminal(&mut world);
    run_terminal(&mut world);
    run_terminal(&mut world);
    assert_eq!(
        answer_of(&world, e),
        "x!",
        "the hook compounded its own output"
    );
}

/// A throwing hook must not be retried next tick, or one error becomes an
/// infinite loop. The marker goes on before the script runs.
#[test]
fn a_failing_terminal_hook_is_not_retried() {
    let mut world = World::new();
    let e = spawn_terminal(
        &mut world,
        r#"fn on_completion(ctx) { throw "boom" }"#,
        "on_completion",
        AgentStatus::Complete,
        Some("x"),
    );
    run_terminal(&mut world);
    let first = status_message(&world, e).expect("errored");
    run_terminal(&mut world);
    assert_eq!(
        status_message(&world, e).expect("still errored"),
        first,
        "a failing hook ran again"
    );
    assert!(world.get::<TerminalHookFired>(e).is_some());
}

#[test]
fn on_completion_can_veto_the_answer() {
    let mut world = World::new();
    let e = spawn_terminal(
        &mut world,
        r#"fn on_completion(ctx) { #{ action: "cancel", reason: "schema mismatch" } }"#,
        "on_completion",
        AgentStatus::Complete,
        Some("x"),
    );
    run_terminal(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("schema mismatch")
    );
}

/// A cancelled run was stopped from outside. Narrating that back to the
/// operator who stopped it is not useful, so neither hook fires.
#[test]
fn a_cancelled_run_fires_no_terminal_hook() {
    let mut world = World::new();
    let e = spawn_terminal(
        &mut world,
        r#"fn on_completion(ctx) { #{ action: "cancel", reason: "should not run" } }"#,
        "on_completion",
        AgentStatus::Cancelled,
        Some("x"),
    );
    run_terminal(&mut world);
    assert!(status_message(&world, e).is_none());
    assert!(world.get::<TerminalHookFired>(e).is_none());
}

/// A run still going fires nothing, and is not marked - it has not finished.
#[test]
fn a_running_agent_fires_no_terminal_hook_and_stays_unmarked() {
    let mut world = World::new();
    let e = spawn_terminal(
        &mut world,
        r#"fn on_completion(ctx) { #{ action: "cancel" } }"#,
        "on_completion",
        AgentStatus::Active,
        Some("x"),
    );
    run_terminal(&mut world);
    assert!(status_message(&world, e).is_none());
    assert!(
        world.get::<TerminalHookFired>(e).is_none(),
        "an unfinished run must stay eligible"
    );
}

/// The completion hook of a run that never submitted an answer sees an empty
/// string, not a missing field.
#[test]
fn on_completion_without_an_answer_sees_empty_output() {
    let mut world = World::new();
    let e = spawn_terminal(
        &mut world,
        r#"fn on_completion(ctx) { if ctx.output == "" { () } else { #{ action: "cancel" } } }"#,
        "on_completion",
        AgentStatus::Complete,
        None,
    );
    run_terminal(&mut world);
    assert!(status_message(&world, e).is_none());
}

#[test]
fn a_terminal_hook_modify_must_be_text() {
    let mut world = World::new();
    let e = spawn_terminal(
        &mut world,
        r#"fn on_completion(ctx) { #{ action: "modify", value: #{ not: "text" } } }"#,
        "on_completion",
        AgentStatus::Complete,
        Some("x"),
    );
    run_terminal(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("replacement text")
    );
}

#[test]
fn a_terminal_hook_retry_is_refused() {
    let mut world = World::new();
    let e = spawn_terminal(
        &mut world,
        r#"fn on_completion(ctx) { #{ action: "retry" } }"#,
        "on_completion",
        AgentStatus::Complete,
        Some("x"),
    );
    run_terminal(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("cannot honour")
    );
}

#[test]
fn a_terminal_hook_veto_without_a_reason_still_explains() {
    let mut world = World::new();
    let e = spawn_terminal(
        &mut world,
        "fn on_completion(ctx) { false }",
        "on_completion",
        AgentStatus::Complete,
        Some("x"),
    );
    run_terminal(&mut world);
    let msg = status_message(&world, e).expect("errored");
    assert!(msg.contains("rejected the result"), "{msg}");
    assert!(msg.contains("no reason given"), "{msg}");
}

#[test]
fn a_terminal_hook_allow_leaves_everything_alone() {
    let mut world = World::new();
    let e = spawn_terminal(
        &mut world,
        "fn on_completion(ctx) { () }",
        "on_completion",
        AgentStatus::Complete,
        Some("x"),
    );
    run_terminal(&mut world);
    assert_eq!(answer_of(&world, e), "x");
    assert!(status_message(&world, e).is_none());
}

/// No stage and no declared hook both mark the agent anyway: re-checking a
/// finished run on every tick is pure work.
#[test]
fn a_terminal_run_with_no_hook_is_marked_so_it_is_not_rechecked() {
    let mut world = World::new();
    let mut state = agent_state();
    state.status = AgentStatus::Complete;
    let stage = leviath_core::Stage::new(
        "main".to_string(),
        leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
    );
    let undeclared = world
        .spawn((
            AgentBlueprint(blueprint(vec![stage])),
            state.clone(),
            StageCursor { index: 0 },
            hook_scripts("fn on_completion(ctx) { () }", &["on_completion"]),
        ))
        .id();
    let out_of_range = world
        .spawn((
            stage_hooked(|h, p| h.on_completion = Some(p)),
            state,
            StageCursor { index: 99 },
            hook_scripts("fn on_completion(ctx) { () }", &["on_completion"]),
        ))
        .id();

    run_terminal(&mut world);
    assert!(world.get::<TerminalHookFired>(undeclared).is_some());
    assert!(world.get::<TerminalHookFired>(out_of_range).is_some());
}

/// Rewriting an answer that was never submitted is refused, not dropped - a
/// silently-ignored rewrite reads exactly like one that happened.
#[test]
fn on_completion_rewriting_a_missing_answer_is_refused() {
    let mut world = World::new();
    let e = spawn_terminal(
        &mut world,
        r#"fn on_completion(ctx) { #{ action: "modify", value: "new" } }"#,
        "on_completion",
        AgentStatus::Complete,
        None,
    );
    run_terminal(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("submitted none")
    );
}

// ─── on_stage_exit (issue #260) ──────────────────────────────────────────────

fn spawn_exiting(world: &mut World, src: &str) -> Entity {
    world
        .spawn((
            stage_hooked(|h, p| h.on_stage_exit = Some(p)),
            agent_state(),
            conv_window(),
            StageCursor { index: 0 },
            ResolveTransition,
            hook_scripts(src, &["on_stage_exit"]),
        ))
        .id()
}

fn run_exit_hooks(world: &mut World) {
    let mut schedule = Schedule::default();
    schedule.add_systems(run_stage_exit_hooks);
    schedule.run(world);
}

/// The point of the hook: summarise or tidy while the finishing stage is still
/// the current one.
#[test]
fn on_stage_exit_can_write_the_finishing_stages_window() {
    let mut world = World::new();
    let e = spawn_exiting(
        &mut world,
        r#"fn on_stage_exit(ctx) { #{ action: "modify", value: #{ conversation: "summary of " + ctx.stage } } }"#,
    );
    run_exit_hooks(&mut world);
    assert_eq!(region_text(&world, e, "conversation"), "summary of main");
}

#[test]
fn on_stage_exit_allow_changes_nothing() {
    let mut world = World::new();
    let e = spawn_exiting(&mut world, "fn on_stage_exit(ctx) { () }");
    run_exit_hooks(&mut world);
    assert_eq!(region_text(&world, e, "conversation"), "");
    assert!(status_message(&world, e).is_none());
}

/// A stage that refuses to be left has nowhere to go, so this stops the run
/// rather than blocking the transition and wedging it.
#[test]
fn on_stage_exit_can_refuse_and_the_run_stops() {
    let mut world = World::new();
    let e = spawn_exiting(
        &mut world,
        r#"fn on_stage_exit(ctx) { #{ action: "cancel", reason: "work unfinished" } }"#,
    );
    run_exit_hooks(&mut world);
    let msg = status_message(&world, e).expect("errored");
    assert!(msg.contains("refused to leave stage 'main'"), "{msg}");
    assert!(msg.contains("work unfinished"), "{msg}");
}

#[test]
fn on_stage_exit_refusing_without_a_reason_still_explains() {
    let mut world = World::new();
    let e = spawn_exiting(&mut world, "fn on_stage_exit(ctx) { false }");
    run_exit_hooks(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("no reason given")
    );
}

#[test]
fn on_stage_exit_that_throws_errors_the_run() {
    let mut world = World::new();
    let e = spawn_exiting(&mut world, r#"fn on_stage_exit(ctx) { throw "no" }"#);
    run_exit_hooks(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("hook failed")
    );
}

#[test]
fn on_stage_exit_retry_is_refused_as_unhonourable() {
    let mut world = World::new();
    let e = spawn_exiting(
        &mut world,
        r#"fn on_stage_exit(ctx) { #{ action: "retry" } }"#,
    );
    run_exit_hooks(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("cannot honour")
    );
}

#[test]
fn on_stage_exit_bad_modify_errors() {
    let mut world = World::new();
    let e = spawn_exiting(
        &mut world,
        r#"fn on_stage_exit(ctx) { #{ action: "modify", value: #{ nope: "x" } } }"#,
    );
    run_exit_hooks(&mut world);
    assert!(
        status_message(&world, e)
            .expect("errored")
            .contains("no region 'nope'")
    );
}

#[test]
fn on_stage_exit_skips_an_out_of_range_stage_and_a_stage_that_declared_none() {
    let mut world = World::new();
    let out_of_range = world
        .spawn((
            stage_hooked(|h, p| h.on_stage_exit = Some(p)),
            agent_state(),
            conv_window(),
            StageCursor { index: 99 },
            ResolveTransition,
            hook_scripts(
                r#"fn on_stage_exit(ctx) { #{ action: "cancel" } }"#,
                &["on_stage_exit"],
            ),
        ))
        .id();
    let stage = leviath_core::Stage::new(
        "main".to_string(),
        leviath_core::blueprint::ModelConfig::new("p".to_string(), "m".to_string()),
    );
    let undeclared = world
        .spawn((
            AgentBlueprint(blueprint(vec![stage])),
            agent_state(),
            conv_window(),
            StageCursor { index: 0 },
            ResolveTransition,
            hook_scripts(
                r#"fn on_stage_exit(ctx) { #{ action: "cancel" } }"#,
                &["on_stage_exit"],
            ),
        ))
        .id();
    run_exit_hooks(&mut world);
    assert!(status_message(&world, out_of_range).is_none());
    assert!(status_message(&world, undeclared).is_none());
}
