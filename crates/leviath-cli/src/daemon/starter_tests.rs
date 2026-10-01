//! Starting runs the daemon's way: everything a start resolves, binds and
//! places, checked on the entity it lands as.

use super::testing::{TestDeps, TestLaunch, start_run};
use super::*;
use crate::config::Config;
use crate::daemon::spawn::tests::custom_region_manifest;
use crate::test_support::{FakeProvider, fixtures};
use leviath_providers::{Provider, Tool};
use leviath_runtime::ProviderRegistry;
use leviath_runtime::components::AgentStatus;
use leviath_runtime::inference_pool::InferencePoolConfig;
use leviath_runtime::interaction_hub::InteractionHub;
use leviath_runtime::persistence::RunMetadata;
use leviath_runtime::pipeline::CompactionSettings;
use leviath_runtime::world::PipelineWorld;
use std::collections::HashMap;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

/// A minimal single-stage manifest with a tiny task region and a `system_prompt`
/// large enough to overflow it, so stage-0 setup fails in `spawn_agent`.
const OVERSIZED_MANIFEST: &str = r#"[blueprint]
name = "tiny"
version = "0.1.0"
description = "d"

[graph]
entry = "main"

[[graph.stages]]
name = "main"
description = "d"
system_prompt = "SYSTEM_PROMPT_PLACEHOLDER"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
regions = [{ name = "task", kind = "pinned", budget = 20 }]
total_budget_tokens = 20

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
binds = [{ region = "task" }]
"#;

const PROFILES_TOML: &str = "[careful]\ndefault = \"ask\"\nquestions = \"ask\"\n\
    checkpoints = \"ask\"\ngate = \"ask\"\n\n[loose]\ndefault = \"allow\"\n";

/// A throwaway sub-agent op sender for tests that don't exercise the bridge.
fn sub_tx() -> UnboundedSender<SubAgentOp> {
    tokio::sync::mpsc::unbounded_channel().0
}

fn registry_with(providers: &[&str]) -> ProviderRegistry {
    let mut r = ProviderRegistry::new();
    for p in providers {
        r.register(p.to_string(), Arc::new(fake_provider()));
    }
    r
}

fn fake_provider() -> FakeProvider {
    FakeProvider::new().failing("test provider")
}

fn coder_manifest() -> String {
    // Self-contained fixture - not the shipped blueprint, so these spawn-logic
    // tests stay isolated from agents/coder edits.
    crate::test_support::inline_coder_manifest()
}

fn test_world() -> (PipelineWorld, Arc<CliToolService>) {
    let cli = Arc::new(CliToolService::new());
    let world = PipelineWorld::new(
        registry_with(&["anthropic", "openai", "ollama"]),
        cli.clone(),
        InferencePoolConfig::new(),
        1,
        None,
        Handle::current(),
    );
    (world, cli)
}

/// [`spawn_args`] with a task, for a blueprint that takes one: a blank
/// task with nothing else handed in is refused.
fn tasked_args(path: &str) -> TestLaunch {
    TestLaunch {
        task: "do the work".to_string(),
        ..spawn_args(path)
    }
}

fn spawn_args(path: &str) -> TestLaunch {
    TestLaunch {
        run_id: "run-x".to_string(),
        blueprint_path: path.to_string(),
        // No task by default: most of these fixtures declare no region to
        // receive one, and supplying a task a blueprint cannot hold is now
        // refused. Tests that care about the task set it explicitly.
        task: String::new(),
        regions: HashMap::new(),
        model: None,
        workdir: std::env::temp_dir().to_string_lossy().to_string(),
        metadata: HashMap::new(),
        callback_url: None,
        callback_secret: None,
        yolo: false,
        yolo_profile: None,
        no_seed_commands: false,
        allow: Vec::new(),
        max_depth: None,
        parent_run_id: None,
        output: None,
        parts: Vec::new(),
        capture_model_input: false,
    }
}

#[tokio::test]
async fn build_agent_fails_fast_on_a_broken_custom_region_script() {
    // The resolve error propagates out of build_agent before any tokens
    // are spent - a hook that silently never ran would change every
    // inference with no signal.
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(&manifest, custom_region_manifest()).unwrap();

    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let args = spawn_args(&manifest.to_string_lossy());
    let err = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &args,
    )
    .unwrap_err();
    assert!(err.contains("layout.regions[0].kind"), "got: {err}");
    assert!(err.contains("hooks/brain.rhai"), "got: {err}");
}

/// A blueprint's mime check that cannot be loaded stops the spawn, the
/// way its other scripts do, before any tokens are spent.
#[tokio::test]
async fn build_agent_fails_fast_on_a_mime_check_it_cannot_load() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "v"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000

[graph.mime_types]
"application/x-acme-scene" = { check = { file = "checks/gone.rhai" } }
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let args = spawn_args(&manifest.to_string_lossy());
    let err = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &args,
    )
    .unwrap_err();
    assert!(err.contains(".check: unresolvable"), "got: {err}");
    assert!(err.contains("gone.rhai"), "got: {err}");
}

/// A required dependency that is not satisfied fails the spawn before any
/// tokens are spent, with a message pointing at `lev deps`.
#[tokio::test]
async fn build_agent_fails_fast_on_an_unmet_dependency() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "v"
version = "0.1.0"
description = "d"

[graph]
dependencies = [{ name = "key", needs = { env = "LEVIATH_DEPS_SPAWN_UNSET_XYZ" } }]

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let args = spawn_args(&manifest.to_string_lossy());
    let err = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &args,
    )
    .unwrap_err();
    assert!(err.contains("dependencies[0]: unresolvable"), "got: {err}");
    assert!(err.contains("LEVIATH_DEPS_SPAWN_UNSET_XYZ"), "got: {err}");
    assert!(err.contains("lev deps"), "got: {err}");
}

/// A file handed in is something to do, so a blank task beside it passes
/// the seeds; a file aimed at a region the agent does not have is then
/// refused by the spawn itself, and that refusal is what the caller hears.
#[tokio::test]
async fn build_agent_reports_a_part_for_a_region_the_agent_lacks() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(&manifest, coder_manifest()).unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let mut args = spawn_args(&manifest.to_string_lossy());
    args.parts.push(leviath_core::mime::InboundPart {
        region: Some("nowhere".to_string()),
        name: "sketch.png".to_string(),
        mime_type: None,
        deliver: None,
        caption: None,
        data: b"\x89PNG\r\n\x1a\nsketch".to_vec(),
    });
    let err = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &args,
    )
    .unwrap_err();
    assert!(err.contains("no region is named \"nowhere\""), "{err}");
}

#[tokio::test]
async fn build_agent_rejects_a_workdir_that_is_missing_or_not_a_directory() {
    // `ToolContext::new` silently keeps a path it can't canonicalize, so
    // without this check a bogus workdir spawns a healthy-looking agent
    // whose every tool call then fails with ENOENT.
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "w"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();
    let not_a_dir = dir.path().join("a-file");
    std::fs::write(&not_a_dir, "x").unwrap();

    for workdir in [
        dir.path()
            .join("does-not-exist")
            .to_string_lossy()
            .to_string(),
        not_a_dir.to_string_lossy().to_string(),
    ] {
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let mut args = spawn_args(&manifest.to_string_lossy());
        args.workdir = workdir.clone();
        let err = start_run(
            world.world_mut(),
            TestDeps {
                tool_service: &cli,
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                subagent_tx: sub_tx(),
            },
            &args,
        )
        .unwrap_err();
        assert!(err.contains("workspace"), "got: {err}");
        assert!(err.contains(&workdir), "got: {err}");
    }
}

/// A real spawn, end to end: the blueprint seeds a region from a tool, and
/// the region the agent starts with holds that tool's answer.
///
/// The unit tests above drive `resolve_seeds` with a stub runner, so they
/// prove the shape but not the wiring. What this one covers is the wiring:
/// that the spawn path builds a runner over the agent's real tools, that
/// the policy layers reach it, and that the result lands in the window
/// rather than in a map nobody reads.
///
/// Multi-thread because the seeded call is awaited on the ambient runtime -
/// see `block_on_daemon`, which is what a daemon spawn does too.
#[tokio::test(flavor = "multi_thread")]
async fn build_agent_seeds_a_region_from_a_real_tool_call() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "seeded"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 5000

[[graph.layout.regions]]
name = "task"
kind = "pinned"
budget = 4000

[[graph.layout.regions]]
name = "environment"
kind = "pinned"
budget = 1000

[graph.layout.regions.seed.tools]
calls = [
    { tool = "current_time", args = {} },
    { tool = "locale_info", args = {} },
]

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
binds = [{ region = "task" }]
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let mut args = tasked_args(&manifest.to_string_lossy());
    args.workdir = dir.path().to_string_lossy().to_string();
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &args,
    )
    .expect("spawn succeeds");

    let window = world
        .world()
        .get::<leviath_runtime::components::ContextWindow>(entity)
        .expect("the agent has a window");
    let region = window
        .regions
        .iter()
        .find(|r| r.name == "environment")
        .expect("the seeded region exists");
    let content: String = region
        .content
        .iter()
        .map(|e| e.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    // Both calls ran, each under its own heading.
    assert!(content.contains("--- current_time ---"), "{content}");
    assert!(content.contains("--- locale_info ---"), "{content}");
    // And it is the tool's real answer, not a placeholder: the clock's
    // reading parses back as the instant it claims to be.
    let (_, after) = content
        .split_once("--- current_time ---")
        .expect("the clock block");
    let (json, _) = after
        .split_once("--- locale_info ---")
        .unwrap_or((after, ""));
    let v: serde_json::Value =
        serde_json::from_str(json.trim()).expect("the clock answered with JSON");
    assert!(
        chrono::DateTime::parse_from_rfc3339(v["utc"].as_str().expect("utc")).is_ok(),
        "{json}"
    );
}

#[tokio::test]
async fn build_agent_attaches_taint_gate_when_security_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "sec"
version = "0.1.0"
description = "d"

[graph]
taint_tracking = true

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args(&manifest.to_string_lossy()),
    )
    .expect("spawn succeeds");

    // Taint opt-in ⇒ gate + sensitivities attached and window tracking on.
    assert!(
        world
            .world()
            .get::<leviath_runtime::TaintGate>(entity)
            .is_some()
    );
    assert!(
        world
            .world()
            .get::<leviath_runtime::pipeline::ToolSensitivities>(entity)
            .is_some()
    );
    assert!(
        world
            .world()
            .get::<leviath_runtime::components::ContextWindow>(entity)
            .unwrap()
            .overall_taint()
            .is_some()
    );
    // Without `--yolo`, the gate stays interactive: no auto-approve marker.
    assert!(
        world
            .world()
            .get::<leviath_runtime::components::GateAutoApprove>(entity)
            .is_none()
    );
}

/// A tool the user granted outright still answers to the taint gate.
///
/// `[tool_permissions]` and the gate are separate layers asking separate
/// questions - "may this agent call `shell` at all" and "may *this* data
/// reach it" - and only `--yolo` waives the second. Granting a tool must
/// not quietly grant the data too: measured live, a run with
/// `shell = "allow"` and a Private read still raised the leak prompt, and
/// denying it kept the command from running.
#[tokio::test]
async fn build_agent_tool_permission_allow_does_not_waive_the_taint_gate() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "sec"
version = "0.1.0"
description = "d"

[graph]
taint_tracking = true

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }
tools = ["shell"]

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();
    let mut config = Config::default();
    config
        .tool_permissions
        .insert("shell".to_string(), crate::config::ToolPolicy::Allow);
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &config,
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args(&manifest.to_string_lossy()),
    )
    .expect("spawn succeeds");

    assert!(
        world
            .world()
            .get::<leviath_runtime::TaintGate>(entity)
            .is_some(),
        "the gate is attached regardless of tool permissions"
    );
    assert!(
        world
            .world()
            .get::<leviath_runtime::components::GateAutoApprove>(entity)
            .is_none(),
        "only --yolo waives the gate; a tool grant does not"
    );
}

#[tokio::test]
async fn build_agent_marks_root_runs_for_titling_but_not_subagents() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        // Titling is gated on a non-empty task, so this blueprint has to
        // accept one - a region named `task` picks it up implicitly.
        r#"[blueprint]
name = "titler"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
regions = [{ name = "task", kind = "pinned", budget = 1000 }]
total_budget_tokens = 1000

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
binds = [{ region = "task" }]
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();

    // Root run with the default-enabled [title] config: marked.
    let root = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &TestLaunch {
            task: "title me".to_string(),
            ..spawn_args(&manifest.to_string_lossy())
        },
    )
    .expect("spawn succeeds");
    assert!(
        world
            .world()
            .get::<leviath_runtime::title::PendingTitle>(root)
            .is_some()
    );

    // A sub-agent run is never marked: titles serve the top-level run list.
    let mut child_args = tasked_args(&manifest.to_string_lossy());
    child_args.run_id = "run-child".to_string();
    child_args.parent_run_id = Some("run-x".to_string());
    let child = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &child_args,
    )
    .expect("spawn succeeds");
    assert!(
        world
            .world()
            .get::<leviath_runtime::title::PendingTitle>(child)
            .is_none()
    );

    // Disabled config: not marked.
    let config = Config {
        title: leviath_core::config::TitleConfig {
            enabled: false,
            provider: None,
            model: None,
        },
        ..Config::default()
    };
    let mut off_args = tasked_args(&manifest.to_string_lossy());
    off_args.run_id = "run-off".to_string();
    let off = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &config,
            shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &off_args,
    )
    .expect("spawn succeeds");
    assert!(
        world
            .world()
            .get::<leviath_runtime::title::PendingTitle>(off)
            .is_none()
    );
}

#[tokio::test]
async fn build_agent_applies_policy_mcp_overrides_to_the_gate() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "sec-ov"
version = "0.1.0"
description = "d"

[graph]
taint_tracking = true

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    // The daemon loads policy.toml into this resource at setup; an
    // [mcp_overrides] entry there must reach the gate attached at spawn,
    // not just `lev policy list` output.
    world
        .world_mut()
        .insert_resource(leviath_runtime::pipeline::PolicyGate(
            leviath_core::PolicyConfig {
                allowlist: Vec::new(),
                mcp_overrides: HashMap::from([(
                    "notes.share".to_string(),
                    leviath_core::policy::McpToolOverride {
                        sensitivity: None,
                        direction: Some("outbound".to_string()),
                        clearance: Some(leviath_core::TaintLevel::Private),
                    },
                )]),
            },
        ));
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args(&manifest.to_string_lossy()),
    )
    .expect("spawn succeeds");

    let gate = world
        .world()
        .get::<leviath_runtime::TaintGate>(entity)
        .expect("gate attached");
    let classification = gate.tool_classification("notes.share");
    assert_eq!(
        classification.direction,
        leviath_core::taint::ToolDirection::Outbound
    );
    assert_eq!(classification.clearance, leviath_core::TaintLevel::Private);
}

#[tokio::test]
async fn build_agent_errors_when_required_caller_region_missing() {
    // A required caller-input region that the request doesn't provide makes
    // build_agent fail (via resolve_seeds) before spawning - no inference.
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "needs"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 12000

[[graph.layout.regions]]
name = "spec"
kind = "pinned"
budget = 2000
required = true

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 20 }
budget = 10000

[[graph.inputs]]
name = "spec"
type = { kind = "text", multiline = true }
required = true
binds = [{ region = "spec" }]
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    // spawn_args() provides only the task, not the required `spec` region.
    let err = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args(&manifest.to_string_lossy()),
    )
    .unwrap_err();
    assert!(err.contains("spec"), "got: {err}");
}

#[tokio::test]
async fn build_agent_attaches_sandbox_when_configured() {
    // A `namespace` sandbox with `on_unavailable = "warn"` builds on every
    // platform without running any external command, so this deterministically
    // exercises the spawn-side sandbox wiring (manager built + attached).
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "sb"
version = "0.1.0"
description = "d"

[graph]
sandbox = { kind = "namespace", on_unavailable = "warn" }

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args(&manifest.to_string_lossy()),
    )
    .expect("spawn succeeds");
    // The agent's tool state carries a sandbox manager.
    let state = cli.take(entity).expect("state registered");
    assert!(state.sandbox.is_some(), "sandbox manager attached");
}

/// The spawn hands `install_tool` the run's MCP tool names as reserved: a
/// script under one of them is dropped at every discovery, so installing
/// it would tell the model a tool exists that it can never call. Refused
/// before anything is compiled or written, so no tools directory is
/// touched here.
#[tokio::test]
async fn build_agent_reserves_mcp_tool_names_for_install_tool() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "rs"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let mcp_defs = vec![Tool {
        name: "acme_search".to_string(),
        description: "d".to_string(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
    }];
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &mcp_defs,
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args(&manifest.to_string_lossy()),
    )
    .expect("spawn succeeds");
    let state = cli.take(entity).expect("state registered");
    let out = state
        .builtins
        .execute(
            "install_tool",
            serde_json::json!({
                "name": "acme_search",
                "source": "// @tool acme_search\n// @description d\n1\n",
            }),
        )
        .await;
    assert!(
        out.contains("'acme_search' is the name of a built-in tool"),
        "{out}"
    );
}

#[tokio::test]
async fn build_agent_errors_when_sandbox_runtime_unavailable() {
    // A container sandbox naming a nonexistent engine fails to start on every
    // platform (no runtime needed), so build_agent surfaces the error - this
    // covers the `?` on `SandboxManager::build` uniformly across OSes,
    // independent of which container runtimes happen to be installed.
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "sb"
version = "0.1.0"
description = "d"

[graph]
sandbox = { kind = "container", image = "x", engine = "leviath-no-such-engine" }

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let err = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args(&manifest.to_string_lossy()),
    )
    .expect_err("a nonexistent engine can't start the container");
    assert!(err.contains("sandbox unavailable"), "got: {err}");
}

#[tokio::test]
async fn build_agent_yolo_attaches_gate_auto_approve_when_taint_on() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "sec"
version = "0.1.0"
description = "d"

[graph]
taint_tracking = true

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let mut args = spawn_args(&manifest.to_string_lossy());
    args.yolo = true;
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &args,
    )
    .expect("spawn succeeds");
    // Taint on + `--yolo` ⇒ gate is auto-approved (marker attached) so a
    // headless run never blocks on a gate prompt.
    assert!(
        world
            .world()
            .get::<leviath_runtime::components::GateAutoApprove>(entity)
            .is_some()
    );
    // ...and likewise for the blueprint's own stage-boundary checkpoints and
    // the agent's `ask_user_*` tools: unattended means unattended.
    assert!(
        world
            .world()
            .get::<leviath_runtime::components::InteractionAutoApprove>(entity)
            .is_some()
    );
    assert!(cli.take(entity).expect("tool state registered").unattended);
    // Recorded on the agent, so the sub-agent and fan-out spawners can pass
    // it down and `meta.json` can carry it across a restart.
    assert!(
        world
            .world()
            .get::<RunMetadata>(entity)
            .expect("run metadata attached")
            .unattended
    );
}

/// The status a `--yolo` run reports is `active`, not `waiting`: nothing
/// should be opening a prompt for it in the first place.
#[tokio::test]
async fn build_agent_yolo_leaves_the_run_active_and_unattended() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "a"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    let mut args = spawn_args(&manifest.to_string_lossy());
    args.yolo = true;
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &InteractionHub::new(),
            subagent_tx: sub_tx(),
        },
        &args,
    )
    .expect("spawn succeeds");

    assert_eq!(
        world.agent_status(world.own_agent(entity)),
        Some(AgentStatus::Active)
    );
    let meta = world
        .world()
        .get::<RunMetadata>(entity)
        .expect("run metadata attached");
    assert!(meta.unattended);
}

/// A stage that kept a human tool through an unattended run has to reach the
/// tool state with that tool in hand: the cut takes it out of the advertised
/// set, and this set is what puts a call to it back in front of a person
/// instead of the auto-answering backend.
/// A validator that will not compile stops the spawn, before any tokens are
/// spent. The only other time the script is read is at the end of the run,
/// which is the worst possible moment to learn the agent cannot hand back
/// its work.
#[tokio::test]
async fn build_agent_refuses_a_validator_that_does_not_compile() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(dir.path().join("shape.rhai"), "fn validate(a, b) { () }").unwrap();
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "v"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }
tools = ["submit_output"]
output = { format = "a2ui", validator = { file = "shape.rhai" } }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();

    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let args = spawn_args(&manifest.to_string_lossy());
    let err = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &args,
    )
    .unwrap_err();

    assert!(err.contains("output.validator: invalid"), "got: {err}");
    assert!(err.contains("exactly one parameter"), "and says why: {err}");
}

/// Compiling a validator at spawn is only half of it: it has to reach the
/// entity, or the script is checked and then never runs, and the run hands
/// back an answer nothing looked at.
#[tokio::test]
async fn build_agent_carries_output_validators_onto_the_entity() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(dir.path().join("shape.rhai"), "fn validate(content) { () }").unwrap();
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "v"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }
tools = ["submit_output"]
output = { format = "a2ui", validator = { file = "shape.rhai" } }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();

    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let args = spawn_args(&manifest.to_string_lossy());
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &args,
    )
    .expect("spawns");

    let validators = world
        .world()
        .get::<leviath_runtime::components::OutputValidators>(entity)
        .expect("the compiled validator reaches the entity");
    assert!(validators.compiled.contains_key("shape.rhai"));
}

/// And an agent that names none carries none, rather than an empty
/// component every consumer then has to check.
#[tokio::test]
async fn build_agent_carries_no_validators_when_none_are_named() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "v"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();

    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let args = spawn_args(&manifest.to_string_lossy());
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &args,
    )
    .expect("spawns");

    assert!(
        world
            .world()
            .get::<leviath_runtime::components::OutputValidators>(entity)
            .is_none()
    );
}

#[tokio::test]
async fn build_agent_carries_required_tools_into_the_tool_state() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "asks"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }
tools = [
    "read_file",
    "ask_user_text",
]
required_tools = ["ask_user_text"]
tool_accepts = { read_file = ["text/*"] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    let mut args = spawn_args(&manifest.to_string_lossy());
    args.yolo = true;
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &InteractionHub::new(),
            subagent_tx: sub_tx(),
        },
        &args,
    )
    .expect("spawn succeeds");

    let state = cli.take(entity).expect("tool state registered");
    assert!(
        state
            .stage_required
            .lock()
            .unwrap()
            .contains("ask_user_text")
    );
    assert_eq!(state.stage_required_by_index.len(), 1);
    // And what the stage lets each tool be handed, by canonical name.
    assert_eq!(
        state.stage_tool_accepts.lock().unwrap().get("read_file"),
        Some(&vec!["text/*".to_string()])
    );
    assert_eq!(state.stage_tool_accepts_by_index.len(), 1);
}

#[tokio::test]
async fn build_agent_without_yolo_keeps_prompts_interactive() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "plain"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args(&manifest.to_string_lossy()),
    )
    .expect("spawn succeeds");
    assert!(
        world
            .world()
            .get::<leviath_runtime::components::InteractionAutoApprove>(entity)
            .is_none()
    );
    assert!(!cli.take(entity).expect("tool state registered").unattended);
}

/// Capture is off for a plain run, on when the machine asked for every run,
/// and on when one spawn asked for itself. Off by default is the safety
/// property, so the absence is asserted as hard as the presence.
#[tokio::test]
async fn the_capture_marker_lands_only_when_the_machine_or_the_spawn_asks() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "plain"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();
    let path = manifest.to_string_lossy().to_string();
    let captured = |config: &Config, args: &TestLaunch| {
        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let entity = start_run(
            world.world_mut(),
            TestDeps {
                tool_service: &cli,
                config,
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                subagent_tx: sub_tx(),
            },
            args,
        )
        .expect("spawn succeeds");
        world
            .world()
            .get::<leviath_runtime::pipeline::CaptureModelInput>(entity)
            .is_some()
    };

    let plain = Config::default();
    assert!(!captured(&plain, &spawn_args(&path)));

    let mut machine_wide = Config::default();
    machine_wide.observability.capture_model_input = true;
    assert!(captured(&machine_wide, &spawn_args(&path)));

    let asked = TestLaunch {
        capture_model_input: true,
        ..spawn_args(&path)
    };
    assert!(captured(&plain, &asked));
}

#[tokio::test]
async fn build_agent_no_security_block_leaves_taint_off_by_default() {
    // A blueprint with no `[security]` block and a default (taint-off)
    // global config must NOT attach the taint gate - an
    // `unwrap_or_default()` on the resolved security forces it on for
    // every agent.
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "plain"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let entity = start_run(
        world.world_mut(),
        TestDeps {
    tool_service: &cli,
    config: &Config::default(),
    shared_mcp: // taint_tracking defaults to false
        mcp,
    mcp_tool_defs: &[],
    mcp_tool_owners: &Default::default(),
    hub: &hub,
    subagent_tx: sub_tx(),
},
        &spawn_args(&manifest.to_string_lossy()),
    )
    .expect("spawn succeeds");
    assert!(
        world
            .world()
            .get::<leviath_runtime::TaintGate>(entity)
            .is_none(),
        "no [security] block + global off ⇒ no taint gate"
    );
}

/// The `no_output_tools` a freshly built agent carries.
async fn spawned_no_output_tools(manifest_body: &str, task: &str) -> bool {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(&manifest, manifest_body).unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &TestLaunch {
            task: task.to_string(),
            ..spawn_args(&manifest.to_string_lossy())
        },
    )
    .expect("spawn succeeds");
    world
        .world()
        .get::<leviath_runtime::persistence::RunOutcomeFlags>(entity)
        .expect("build_agent attaches run outcome flags")
        .0
        .no_output_tools
}

#[tokio::test]
async fn build_agent_records_whether_the_blueprint_can_write_at_all() {
    // A coding agent writes in `implement`, so silence from it is worth
    // reporting.
    assert!(!spawned_no_output_tools(&coder_manifest(), "do the work").await);
    // A router-shaped agent delegates and never writes. Reporting it as
    // having "modified nothing" is an accusation the framework has no
    // grounds for.
    assert!(
        spawned_no_output_tools(
            r#"[blueprint]
name = "router"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "triage"
model = { models = [{ provider = "anthropic", model = "m" }] }
tools = [
    "read_file",
    "spawn_agent",
]

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
            "",
        )
        .await
    );
}

#[tokio::test]
async fn build_agent_spawns_registers_and_wires_tools() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(&manifest, coder_manifest()).unwrap();

    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &tasked_args(&manifest.to_string_lossy()),
    )
    .expect("spawn succeeds");

    assert_eq!(
        world.agent_status(world.own_agent(entity)),
        Some(AgentStatus::Active)
    );
    // The run metadata was attached.
    let md = world
        .world()
        .get::<RunMetadata>(entity)
        .expect("run metadata");
    assert!(md.run_id.starts_with("coder-"), "{}", md.run_id);
    assert_eq!(md.agent_name, "coder");
    // Tool state was registered: a tool batch dispatches (not "no tool state").
    let out = leviath_runtime::pipeline::ToolService::exec_for(
        cli.as_ref(),
        entity,
        vec![leviath_providers::ToolCall {
            id: "c1".to_string(),
            name: "list_dir".to_string(),
            arguments: serde_json::json!({"path": "."}),
            thought_signature: None,
        }],
        leviath_runtime::pipeline::noop_progress(),
    )()
    .await;
    assert_eq!(out[0].0, "c1");
    assert!(!out[0].1.contains("no tool state"));
}

/// Each `tool_rescan` value tags the agent with what that value turns on,
/// and nothing more.
///
/// The markers are what the runtime queries, so a value that tagged too
/// little would leave a run doing less than its blueprint asked for, and one
/// that tagged too much would make every batch of an ordinary run pay for a
/// mode it never asked for.
#[tokio::test]
async fn build_agent_tags_an_agent_with_the_rescan_it_asked_for() {
    use leviath_runtime::pipeline::{DynamicTools, RescanBeforeDispatch};
    for (word, polls, before_dispatch) in [
        ("at_spawn", false, false),
        ("after_writes", true, false),
        ("before_dispatch", true, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.toml");
        std::fs::write(
            &manifest,
            coder_manifest().replace("[graph]\n", &format!("[graph]\ntool_rescan = \"{word}\"\n")),
        )
        .unwrap();

        let (mut world, cli) = test_world();
        let hub = InteractionHub::new();
        let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
        let entity = start_run(
            world.world_mut(),
            TestDeps {
                tool_service: &cli,
                config: &Config::default(),
                shared_mcp: mcp,
                mcp_tool_defs: &[],
                mcp_tool_owners: &Default::default(),
                hub: &hub,
                subagent_tx: sub_tx(),
            },
            &tasked_args(&manifest.to_string_lossy()),
        )
        .expect("spawn succeeds");

        assert_eq!(
            world.world().get::<DynamicTools>(entity).is_some(),
            polls,
            "{word}: whether the runtime polls it between turns"
        );
        assert_eq!(
            world.world().get::<RescanBeforeDispatch>(entity).is_some(),
            before_dispatch,
            "{word}: whether it looks again before each batch"
        );
        // And the re-resolution context exists exactly when it is used.
        assert_eq!(
            leviath_runtime::pipeline::ToolService::refresh_tools(cli.as_ref(), entity, 0)
                .is_some(),
            polls,
            "{word}: whether there is anything to refresh with"
        );
    }
}

#[tokio::test]
async fn build_agent_applies_yolo_allow_and_max_depth() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(&manifest, coder_manifest()).unwrap();

    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    // The user's config denies read_file. Neither `--yolo` nor an explicit
    // `--allow read_file` lifts that: a deny rule is a decision, and skipping
    // *prompts* is all `--yolo` is for.
    let config = Config {
        tool_permissions: HashMap::from([(
            "read_file".to_string(),
            crate::config::ToolPolicy::Deny,
        )]),
        ..Default::default()
    };
    let mut args = tasked_args(&manifest.to_string_lossy());
    args.yolo = true;
    args.allow = vec!["read_file".to_string()];
    args.max_depth = Some(7);

    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &config,
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &args,
    )
    .expect("spawn succeeds");
    assert_eq!(
        world.agent_status(world.own_agent(entity)),
        Some(AgentStatus::Active)
    );

    // The config deny stands: read_file is refused, not executed.
    let out = leviath_runtime::pipeline::ToolService::exec_for(
        cli.as_ref(),
        entity,
        vec![leviath_providers::ToolCall {
            id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "/no/such/file"}),
            thought_signature: None,
        }],
        leviath_runtime::pipeline::noop_progress(),
    )()
    .await;
    let result = out[0].1.clone();
    assert!(
        result.contains("[denied]"),
        "a configured deny must survive --yolo, got: {result}"
    );

    // `--yolo` still does its job for a tool the config did not deny:
    // `list_dir` runs unattended with no approval prompt.
    let out = leviath_runtime::pipeline::ToolService::exec_for(
        cli.as_ref(),
        entity,
        vec![leviath_providers::ToolCall {
            id: "c2".to_string(),
            name: "list_dir".to_string(),
            arguments: serde_json::json!({"path": "."}),
            thought_signature: None,
        }],
        leviath_runtime::pipeline::noop_progress(),
    )()
    .await;
    let result = out[0].1.clone();
    assert!(
        !result.contains("[denied]"),
        "--yolo must still waive approval where nothing denies, got: {result}"
    );
}

#[tokio::test]
async fn build_agent_honors_agent_level_tool_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    // A top-level `[tool_permissions]` block denying a builtin - no stage
    // perms, no launch overrides, no global config deny. Only the agent-level
    // layer can produce the deny, so this proves it is wired through.
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "perm"
version = "0.1.0"
description = "d"

[graph]
tool_permissions = { read_file = "deny" }

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();

    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args(&manifest.to_string_lossy()),
    )
    .expect("spawn succeeds");

    let out = leviath_runtime::pipeline::ToolService::exec_for(
        cli.as_ref(),
        entity,
        vec![leviath_providers::ToolCall {
            id: "c1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "/no/such/file"}),
            thought_signature: None,
        }],
        leviath_runtime::pipeline::noop_progress(),
    )()
    .await;
    assert!(
        out[0].1.contains("[denied]"),
        "agent-level deny should block read_file"
    );
}

#[tokio::test]
async fn build_agent_script_host_honors_agent_level_grants() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "scriptperm"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();

    // `write_file` defaults to Ask, and a script-permission `Inherit`
    // permits the host function only on a hard Allow. The grant below
    // lives solely in the user's per-agent block, so the script host can
    // only see it through the agent-scoped ceiling - the raw global
    // `[tool_permissions]` map is empty here.
    let mut config = Config::default();
    config.agent_tool_permissions.insert(
        "scriptperm".to_string(),
        HashMap::from([("write_file".to_string(), crate::config::ToolPolicy::Allow)]),
    );

    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let mut args = spawn_args(&manifest.to_string_lossy());
    args.workdir = dir.path().to_string_lossy().to_string();
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &config,
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &args,
    )
    .expect("spawn succeeds");

    let state = cli.take(entity).expect("tool state registered at spawn");
    state
        .script_host
        .write_file("granted.txt", "ok")
        .expect("agent-level write_file grant must reach the script host");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("granted.txt")).unwrap(),
        "ok"
    );
}

#[tokio::test]
async fn build_agent_applies_default_max_iterations_only_when_stage_omits_it() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    // Two stages: one omits max_iterations, one sets it explicitly to 3.
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "iters"
version = "0.1.0"
description = "d"

[graph]
edges = [{ name = "next", from = "main", to = "capped" }]

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[[graph.stages]]
name = "capped"
model = { models = [{ provider = "anthropic", model = "m" }] }
max_iterations = 3

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();

    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    // A non-default cap so the assertion can't accidentally match the built-in.
    let config = Config {
        limits: crate::config::LimitsConfig {
            default_max_iterations: Some(42),
            ..Default::default()
        },
        ..Default::default()
    };
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &config,
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args(&manifest.to_string_lossy()),
    )
    .expect("spawn succeeds");

    let spec = world
        .world()
        .get::<leviath_runtime::insert::RunSpecC>(entity)
        .expect("spec");
    let by_name = |n: &str| {
        spec.0
            .graph
            .stages
            .iter()
            .find(|s| s.name.as_str() == n)
            .unwrap()
            .max_iterations
    };
    // The stage that omitted it inherits the config default …
    assert_eq!(by_name("main"), Some(42));
    // … while an explicit per-stage cap is left untouched.
    assert_eq!(by_name("capped"), Some(3));
}

#[tokio::test]
async fn build_agent_leaves_max_iterations_unset_when_config_default_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "nolimit"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();

    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    // `None` disables the config default entirely - the stage stays uncapped.
    let config = Config {
        limits: crate::config::LimitsConfig {
            default_max_iterations: None,
            ..Default::default()
        },
        ..Default::default()
    };
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &config,
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args(&manifest.to_string_lossy()),
    )
    .expect("spawn succeeds");

    let spec = world
        .world()
        .get::<leviath_runtime::insert::RunSpecC>(entity)
        .expect("spec");
    assert_eq!(spec.0.graph.stages[0].max_iterations, None);
}

#[tokio::test]
async fn fake_provider_methods_are_exercised() {
    let p = fake_provider();
    assert_eq!(p.name(), "fake");
    assert_eq!(p.count_tokens("t", "m").await, 1);
    assert_eq!(p.max_context_tokens("m"), 1000);
    let _ = p.capabilities("m");
    assert!(p.infer(&fixtures::inference_request()).await.is_err());
}

#[tokio::test]
async fn build_agent_read_error() {
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let err = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args("/no/such/manifest.leviath"),
    )
    .unwrap_err();
    assert!(err.contains("Could not find a blueprint"), "{err}");
}

#[tokio::test]
async fn build_agent_propagates_spawn_error() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    // A huge prompt that cannot fit the 20-token "task" region.
    let content = OVERSIZED_MANIFEST.replace("SYSTEM_PROMPT_PLACEHOLDER", &"x ".repeat(5000));
    std::fs::write(&manifest, content).unwrap();

    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let result = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args(&manifest.to_string_lossy()),
    );
    assert!(result.is_err(), "expected spawn error, got {result:?}");
}

#[tokio::test]
async fn build_agent_refuses_a_manifest_with_no_usable_provider() {
    // End to end: without this an agent is built pointed at a provider
    // nothing answers to, and then sits at iteration 0 for the life of the
    // daemon.
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "ghostly"
version = "0.1.0"
description = "d"

[graph]
entry = "main"

[[graph.stages]]
name = "main"
description = "d"

[graph.stages.model]
models = [{ provider = "ghost", model = "m" }]
allow_user_default = false

[graph.layout]
regions = [{ name = "task", kind = "pinned", budget = 4000 }]
total_budget_tokens = 4000

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
binds = [{ region = "task" }]
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let err = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args(&manifest.to_string_lossy()),
    )
    .unwrap_err();
    assert!(err.contains("main"), "names the stage: {err}");
    assert!(err.contains("ghost"), "names what it tried: {err}");
}

#[tokio::test]
async fn build_agent_invalid_blueprint() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    // The entry names a stage that does not exist, so the graph does not
    // hold together.
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "bad"
version = "0.1.0"
description = "d"

[graph]
entry = "ghost"

[[graph.stages]]
name = "main"
description = "d"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
regions = [{ name = "task", kind = "pinned", budget = 4000 }]
total_budget_tokens = 4000

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
binds = [{ region = "task" }]
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let err = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args(&manifest.to_string_lossy()),
    )
    .unwrap_err();
    assert!(err.contains("ghost"), "{err}");
}

#[tokio::test]
async fn build_agent_without_entry_stage_and_with_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    // No entry_stage (falls back to the first stage) + a compaction section.
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "mini"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
description = "d"
system_prompt = "be brief"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
regions = [{ name = "task", kind = "pinned", budget = 4000 }]
total_budget_tokens = 4000

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
binds = [{ region = "task" }]

[graph.compaction]
model = { provider = "anthropic", model = "claude-x" }
max_summary_tokens = 2000
temperature = 0.20000000298023224
"#,
    )
    .unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &tasked_args(&manifest.to_string_lossy()),
    )
    .expect("spawn succeeds");
    assert_eq!(
        world.agent_status(world.own_agent(entity)),
        Some(AgentStatus::Active)
    );
    // Compaction settings were attached.
    assert!(world.world().get::<CompactionSettings>(entity).is_some());
}

/// Write a blueprint declaring an out-of-workdir read. Used by the wiring
/// tests below.
fn write_read_paths_manifest(dir: &std::path::Path, allow: &str) -> std::path::PathBuf {
    let manifest = dir.join("agent.toml");
    std::fs::write(
        &manifest,
        format!(
            r#"
[blueprint]
name = "reader"
version = "0.1.0"
description = "d"

[graph]
read_paths = [{allow}]
layout = {{ total_budget_tokens = 4000, regions = [{{ name = "task", kind = "pinned", budget = 4000 }}] }}
inputs = [{{ name = "task", type = "text", binds = [{{ region = "task" }}] }}]
stages = [{{ name = "main", description = "d", model = {{ models = [{{ provider = "anthropic", model = "m" }}] }}, system_prompt = "be brief" }}]
"#
        ),
    )
    .unwrap();
    manifest
}

/// A blueprint declaring a stage hook spawns with the compiled script
/// attached - the branch that only runs when some stage declared one.
#[tokio::test]
async fn build_agent_attaches_declared_stage_hooks() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("h.rhai"), "fn on_stage_enter(ctx) { () }").unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "h"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }
hooks = { on_stage_enter = { file = "h.rhai" } }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();

    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args(&manifest.to_string_lossy()),
    )
    .expect("spawn succeeds");

    let scripts = world
        .world_mut()
        .get::<leviath_runtime::components::StageHookScripts>(entity)
        .expect("the hook script is attached");
    assert!(scripts.0.contains_key("h.rhai"));
}

/// A broken hook script fails the spawn rather than the run - the `?` on
/// the resolver, which is the whole point of resolving at spawn.
#[tokio::test]
async fn build_agent_refuses_a_broken_stage_hook() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("h.rhai"), "fn on_stage_enter(ctx) {").unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(
        &manifest,
        r#"[blueprint]
name = "h"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }
hooks = { on_stage_enter = { file = "h.rhai" } }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
    )
    .unwrap();

    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let err = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args(&manifest.to_string_lossy()),
    )
    .expect_err("a broken hook script must fail the spawn");
    assert!(err.contains("Script compilation failed"), "{err}");
}

/// A granted `[read_paths]` spawns cleanly, with taint on so the read-tool
/// sensitivity bump path runs end to end.
#[tokio::test]
async fn build_agent_wires_granted_read_paths() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_read_paths_manifest(dir.path(), "\"/tmp\"");
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let mut config = Config::default();
    config.security.allow_blueprint_read_paths = true;
    config.taint_tracking = true;
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &config,
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &tasked_args(&manifest.to_string_lossy()),
    )
    .expect("spawn succeeds");
    assert_eq!(
        world.agent_status(world.own_agent(entity)),
        Some(AgentStatus::Active)
    );
}

/// A declared-but-ungranted `[read_paths]` still spawns; the warning-logging
/// branch fires.
#[tokio::test]
async fn build_agent_wires_ungranted_read_paths() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_read_paths_manifest(dir.path(), "\"/tmp\"");
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let entity = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &tasked_args(&manifest.to_string_lossy()),
    )
    .expect("spawn succeeds even when nothing grants the declaration");
    assert_eq!(
        world.agent_status(world.own_agent(entity)),
        Some(AgentStatus::Active)
    );
}

/// A malformed grant entry in the user's own config fails the spawn - the
/// error propagates out of `build_read_path_policy`.
#[tokio::test]
async fn build_agent_rejects_a_malformed_config_grant() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_read_paths_manifest(dir.path(), "\"/tmp\"");
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let mut config = Config::default();
    config.security.read_paths = vec!["glob:[".to_string()];
    let err = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &config,
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &tasked_args(&manifest.to_string_lossy()),
    )
    .expect_err("a broken config grant must fail the spawn");
    assert!(err.contains("config.toml"), "{err}");
}

#[tokio::test]
async fn build_agent_parse_error() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("agent.toml");
    std::fs::write(&manifest, "this is not valid toml : : :").unwrap();
    let (mut world, cli) = test_world();
    let hub = InteractionHub::new();
    let mcp = Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new()));
    let err = start_run(
        world.world_mut(),
        TestDeps {
            tool_service: &cli,
            config: &Config::default(),
            shared_mcp: mcp,
            mcp_tool_defs: &[],
            mcp_tool_owners: &Default::default(),
            hub: &hub,
            subagent_tx: sub_tx(),
        },
        &spawn_args(&manifest.to_string_lossy()),
    )
    .unwrap_err();
    assert!(err.contains("is not a valid blueprint"), "{err}");
}

/// A profile that keeps the human mechanisms leaves every marker off: the
/// run is still yolo (its tool calls answer to the profile), but a
/// checkpoint opens, the gate asks, and the model's questions are offered.
/// One that keeps nothing is bare `--yolo` with a name on it.
#[tokio::test]
async fn build_agent_under_a_profile_keeps_what_the_profile_keeps() {
    crate::config::with_isolated_config_path_async("spawn_profile_keeps", |cfg| async move {
        std::fs::write(cfg.join("yolo.toml"), PROFILES_TOML).unwrap();
        for (name, auto) in [("careful", false), ("loose", true)] {
            let dir = tempfile::tempdir().unwrap();
            let manifest = dir.path().join("agent.toml");
            std::fs::write(
                &manifest,
                r#"[blueprint]
name = "sec"
version = "0.1.0"
description = "d"

[graph]
taint_tracking = true

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
            )
            .unwrap();
            let (mut world, cli) = test_world();
            let hub = InteractionHub::new();
            let mut args = spawn_args(&manifest.to_string_lossy());
            args.yolo = true;
            args.yolo_profile = Some(name.to_string());
            let entity = start_run(
                world.world_mut(),
                TestDeps {
                    tool_service: &cli,
                    config: &Config::default(),
                    shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                    mcp_tool_defs: &[],
                    mcp_tool_owners: &Default::default(),
                    hub: &hub,
                    subagent_tx: sub_tx(),
                },
                &args,
            )
            .expect("spawn succeeds");
            assert_eq!(
                world
                    .world()
                    .get::<leviath_runtime::components::GateAutoApprove>(entity)
                    .is_some(),
                auto,
                "{name}: gate marker"
            );
            assert_eq!(
                world
                    .world()
                    .get::<leviath_runtime::components::InteractionAutoApprove>(entity)
                    .is_some(),
                auto,
                "{name}: checkpoint marker"
            );
            let meta = world
                .world()
                .get::<RunMetadata>(entity)
                .expect("run metadata attached");
            assert!(meta.unattended, "{name}: still a yolo run");
            assert_eq!(meta.yolo_profile.as_deref(), Some(name));
            let state = cli.take(entity).expect("tool state registered");
            assert_eq!(state.unattended, auto, "{name}: questions routing");
            let profile = state.yolo.get();
            assert_eq!(
                profile.as_ref().as_ref().map(|p| p.name.as_str()),
                Some(name)
            );
            let handle = state.subagent.as_ref().expect("a sub-agent handle");
            assert!(handle.unattended);
            assert_eq!(handle.yolo_profile.as_deref(), Some(name));
        }
    })
    .await;
}

/// A name the file does not have stops the spawn and lists what it does
/// have. Without `yolo`, a stray name is not even looked up.
#[tokio::test]
async fn build_agent_refuses_a_profile_the_file_does_not_have() {
    crate::config::with_isolated_config_path_async("spawn_profile_unknown", |cfg| async move {
        std::fs::write(cfg.join("yolo.toml"), PROFILES_TOML).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("agent.toml");
        std::fs::write(
            &manifest,
            r#"[blueprint]
name = "a"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 10 }
budget = 10000
"#,
        )
        .unwrap();
        let (mut world, cli) = test_world();
        let config = Config::default();
        let hub = InteractionHub::new();
        let owners = Default::default();
        let deps = || TestDeps {
            tool_service: &cli,
            config: &config,
            shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
            mcp_tool_defs: &[],
            mcp_tool_owners: &owners,
            hub: &hub,
            subagent_tx: sub_tx(),
        };
        let mut args = spawn_args(&manifest.to_string_lossy());
        args.yolo = true;
        args.yolo_profile = Some("nope".to_string());
        let err = start_run(world.world_mut(), deps(), &args).expect_err("unknown profile");
        assert!(err.contains("no yolo profile named \"nope\""), "{err}");
        assert!(err.contains("careful, loose"), "{err}");

        std::fs::remove_file(cfg.join("yolo.toml")).unwrap();
        args.yolo = false;
        let entity =
            start_run(world.world_mut(), deps(), &args).expect("an attended run ignores the name");
        let meta = world.world().get::<RunMetadata>(entity).expect("metadata");
        assert!(!meta.unattended);
        assert!(meta.yolo_profile.is_none());
        assert!(cli.take(entity).expect("state").yolo.get().is_none());
    })
    .await;
}

/// A seed answers to the profile as a mid-run call does. Bare `--yolo`
/// is what lets a seeded `shell` run at all - a seed refuses `ask` - and a
/// profile whose default asks leaves the region empty rather than running
/// it.
#[tokio::test(flavor = "multi_thread")]
async fn a_tool_seed_answers_to_the_yolo_profile() {
    crate::config::with_isolated_config_path_async("spawn_profile_seed", |cfg| async move {
        std::fs::write(cfg.join("yolo.toml"), PROFILES_TOML).unwrap();
        for (profile, expect_ran) in [
            (None, true),
            (Some("careful"), false),
            (Some("loose"), true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let manifest = dir.path().join("agent.toml");
            std::fs::write(
                &manifest,
                r#"[blueprint]
name = "seeded"
version = "0.1.0"
description = "d"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "m" }] }

[graph.layout]
total_budget_tokens = 5000

[[graph.layout.regions]]
name = "task"
kind = "pinned"
budget = 4000

[[graph.layout.regions]]
name = "environment"
kind = "pinned"
budget = 1000

[graph.layout.regions.seed]
tools = { calls = [{ tool = "shell", args = { command = "echo seeded" } }] }

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
binds = [{ region = "task" }]
"#,
            )
            .unwrap();
            let (mut world, cli) = test_world();
            let mut args = tasked_args(&manifest.to_string_lossy());
            args.workdir = dir.path().to_string_lossy().to_string();
            args.yolo = true;
            args.yolo_profile = profile.map(str::to_string);
            let entity = start_run(
                world.world_mut(),
                TestDeps {
                    tool_service: &cli,
                    config: &Config::default(),
                    shared_mcp: Arc::new(Mutex::new(leviath_mcp::ToolExecutor::new())),
                    mcp_tool_defs: &[],
                    mcp_tool_owners: &Default::default(),
                    hub: &InteractionHub::new(),
                    subagent_tx: sub_tx(),
                },
                &args,
            )
            .expect("spawn succeeds");
            let window = world
                .world()
                .get::<leviath_runtime::components::ContextWindow>(entity)
                .expect("the agent has a window");
            let content: String = window
                .regions
                .iter()
                .find(|r| r.name == "environment")
                .expect("the seeded region exists")
                .content
                .iter()
                .map(|e| e.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(
                content.contains("seeded"),
                expect_ran,
                "{profile:?}: {content}"
            );
        }
    })
    .await;
}
