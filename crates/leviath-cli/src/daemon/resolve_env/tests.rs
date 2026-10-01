use super::*;
use crate::test_support::FakeProvider;
use leviath_runtime::spec::graph::{ToolGroup, ToolSelector};
use leviath_runtime::spec::names::ToolName;

/// A small valid manifest: two stages, one without an iteration ceiling.
pub(crate) const MANIFEST: &str = r#"[agent]
name = "helper"
version = "1.2.3"

[stages.plan]
mode = "autonomous"
model = { provider = "mock", model = "m" }
available_tools = ["read_file"]
max_iterations = 5
[stages.plan.transitions.work]
transform = "direct"

[stages.work]
mode = "autonomous"
model = { provider = "mock", model = "m" }
available_tools = ["read_file", "write_file"]
max_iterations = 0

[context.regions]
system = { kind = "pinned", max_tokens = 1000 }
task = { kind = "pinned", max_tokens = 1000, seed = { caller_input = "task" } }
"#;

/// A tool definition with a plain schema.
pub(crate) fn tool(name: &str) -> Tool {
    Tool {
        name: name.into(),
        description: format!("{name} does things"),
        parameters: serde_json::json!({"type": "object"}),
    }
}

/// A daemon env over `config`, with a `mock` provider, one connected MCP
/// server `gh` offering `gh__search`, and an empty agents directory.
pub(crate) fn env_with(config: Config) -> (DaemonEnv, tempfile::TempDir) {
    let agents = tempfile::tempdir().unwrap();
    let mut registry = leviath_runtime::ProviderRegistry::new();
    registry.register("mock".into(), Arc::new(FakeProvider::new()));
    let env = DaemonEnv {
        config: Arc::new(config),
        registry,
        agents_dir: Some(agents.path().to_path_buf()),
        workdir_root: None,
        mcp_defs: vec![tool("gh__search"), tool("orphan")],
        mcp_owners: [("gh__search".to_string(), "gh".to_string())].into(),
        shared_mcp: Arc::new(tokio::sync::Mutex::new(leviath_mcp::ToolExecutor::new())),
        tool_service: Arc::new(CliToolService::new()),
        hub: InteractionHub::new(),
        subagent_tx: tokio::sync::mpsc::unbounded_channel().0,
        mime: Arc::new(leviath_core::mime::MimeRegistry::builtin()),
    };
    (env, agents)
}

pub(crate) fn env() -> (DaemonEnv, tempfile::TempDir) {
    env_with(Config::default())
}

/// Install `manifest` as `name` under the env's agents directory.
pub(crate) fn install(agents: &tempfile::TempDir, name: &str, manifest: &str) -> PathBuf {
    let dir = agents.path().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    crate::test_support::write_test_agent(&dir, manifest)
}

fn reference(text: &str) -> BlueprintRef {
    BlueprintRef::parse(text).unwrap()
}

#[tokio::test]
async fn an_installed_blueprint_loads_as_a_graph_pinned_to_what_was_read() {
    let mut config = Config::default();
    config.limits.default_max_iterations = Some(7);
    let (env, agents) = env_with(config);
    let manifest = install(&agents, "helper", MANIFEST);
    let loaded = env.blueprint(&reference("helper")).await.unwrap();
    let digest = Digest::of(MANIFEST.as_bytes());
    assert_eq!(loaded.reference.digest, Some(digest.clone()));
    assert_eq!(loaded.version, "1.2.3");
    assert_eq!(loaded.base_dir, manifest.parent().unwrap());
    let ceilings: Vec<Option<u32>> = loaded
        .graph
        .stages
        .iter()
        .map(|s| s.max_iterations)
        .collect();
    assert_eq!(
        ceilings,
        [Some(5), Some(7)],
        "0 takes the operator's ceiling"
    );
    assert!(
        env.blueprint(&reference(&format!("helper@{digest}")))
            .await
            .is_ok()
    );

    let moved = env
        .blueprint(&reference(&format!("helper@{}", Digest::of(b"older"))))
        .await
        .unwrap_err();
    assert_eq!(moved.code, IssueCode::Unresolvable);
    assert_eq!(moved.got, Some(format!("revision {digest}")));

    let (unbounded, agents) = env_with({
        let mut c = Config::default();
        c.limits.default_max_iterations = None;
        c
    });
    install(&agents, "helper", MANIFEST);
    let loaded = unbounded.blueprint(&reference("helper")).await.unwrap();
    assert_eq!(loaded.graph.stages[1].max_iterations, Some(0));
}

#[tokio::test]
async fn a_blueprint_that_is_missing_or_will_not_load_is_an_issue() {
    let (env, agents) = env();
    install(&agents, "helper", MANIFEST);
    std::fs::create_dir_all(agents.path().join("empty-dir")).unwrap();
    let missing = env.blueprint(&reference("nope")).await.unwrap_err();
    assert_eq!(missing.path.to_string(), "source.blueprint");
    assert_eq!(missing.known, ["helper"]);

    install(&agents, "broken", "this is not toml [");
    let broken = env.blueprint(&reference("broken")).await.unwrap_err();
    assert!(broken.message.starts_with("parse manifest"), "{broken}");

    install(
        &agents,
        "invalid",
        &MANIFEST.replace(
            "version = \"1.2.3\"",
            "version = \"1.2.3\"\nentry_stage = \"ghost\"",
        ),
    );
    let invalid = env.blueprint(&reference("invalid")).await.unwrap_err();
    assert!(
        invalid.message.starts_with("invalid blueprint"),
        "{invalid}"
    );

    install(
        &agents,
        "odd",
        &MANIFEST.replace(
            "available_tools = [\"read_file\"]",
            "available_tools = [\"no spaces allowed\"]",
        ),
    );
    let odd = env.blueprint(&reference("odd")).await.unwrap_err();
    assert!(
        odd.message.contains("does not read as a run graph"),
        "{odd}"
    );

    let nowhere = DaemonEnv {
        agents_dir: None,
        ..env
    };
    let none = nowhere.blueprint(&reference("helper")).await.unwrap_err();
    assert!(none.known.is_empty());
}

#[test]
fn limits_and_run_ids_come_from_the_operator_and_the_title() {
    let mut config = Config::default();
    config.security.allow_seed_commands = false;
    let (env, _agents) = env_with(config.clone());
    let limits = env.limits();
    assert_eq!(limits.default_max_depth, 3);
    assert!(!limits.seed_commands_allowed);
    assert_eq!(limits.max_attachment_bytes, config.max_part_bytes());

    let id = env.new_run_id(&"Review The Thing ".repeat(10));
    assert!(id.as_str().starts_with("Review-The-Thing-"), "{id}");
    assert!(id.as_str().len() < 100, "{id}");
    assert!(env.new_run_id("").as_str().starts_with("run-"));
}

#[test]
fn a_workdir_must_exist_and_stay_under_the_root() {
    let (mut env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    let none = env.workdir(None).unwrap_err();
    assert!(none.contains("names its workdir"), "{none}");
    let missing = env.workdir(Some(&dir.path().join("gone"))).unwrap_err();
    assert!(missing.contains("does not exist"), "{missing}");
    assert_eq!(env.workdir(Some(dir.path())).unwrap(), canonical);
    env.workdir_root = Some(canonical.clone());
    std::fs::create_dir(dir.path().join("inner")).unwrap();
    assert!(env.workdir(Some(&dir.path().join("inner"))).is_ok());
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = env.workdir(Some(elsewhere.path())).unwrap_err();
    assert!(outside.contains("--workdir-root"), "{outside}");
}

#[tokio::test]
async fn paths_models_code_and_bytes_answer_through_the_shared_host() {
    let (env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    let p = WorkdirPath::new("a.txt").unwrap();
    assert!(env.path_exists(dir.path(), &p, PathKind::File));
    assert!(!env.path_exists(dir.path(), &p, PathKind::Dir));

    let graph = load_installed_graph();
    let plan = env.model(&graph.stages[0], None).await.unwrap();
    assert_eq!((plan.provider.as_str(), plan.model.as_str()), ("mock", "m"));
    let mut nowhere = graph.stages[0].clone();
    nowhere.model.models = vec![ModelRef::parse("gone/x").unwrap()];
    nowhere.model.allow_user_default = false;
    let issue = env.model(&nowhere, None).await.unwrap_err();
    assert_eq!(issue.path.to_string(), "stages.plan.model");

    assert_eq!(
        env.code(&CodeRef::Inline("x".into()), None).await.unwrap(),
        b"x"
    );
    assert!(env.check_code(b"let x = 1; x", CodeUse::Seed).is_ok());
    let unparsable = env.check_code(b"let = ;", CodeUse::Seed).unwrap_err();
    assert!(!unparsable.is_empty());
    assert!(env.check_code(&[0xff], CodeUse::Seed).is_err());
    assert!(
        env.check_code(b"fn check() { () }", CodeUse::DependencyCheck)
            .is_ok()
    );
    assert_eq!(env.sniff("a.txt", b"hi", None).unwrap(), "text/plain");
}

/// The test manifest's graph, read the way a spawn reads it.
fn load_installed_graph() -> RunGraph {
    let (_, agents) = env();
    install(&agents, "helper", MANIFEST);
    load_installed(
        Some(agents.path()),
        &reference("helper"),
        &Config::default(),
    )
    .unwrap()
    .graph
}

fn names(tools: &[ToolDef]) -> Vec<String> {
    tools.iter().map(|t| t.name.to_string()).collect()
}

#[tokio::test]
async fn the_catalog_holds_every_kind_of_tool_and_a_stage_gets_what_it_names() {
    let home = tempfile::tempdir().unwrap();
    let tools_dir = home.path().join(".leviath").join("tools");
    std::fs::create_dir_all(&tools_dir).unwrap();
    std::fs::write(tools_dir.join("global.rhai"), "// @tool global_tool\n1").unwrap();
    std::fs::write(tools_dir.join("shadow.rhai"), "// @tool read_file\n1").unwrap();
    std::fs::write(
        tools_dir.join("needs.rhai"),
        "// @tool needy\n// @requires teleport\n1",
    )
    .unwrap();
    let (env, _agents) = env();
    let own = "// @tool own_tool\n2";
    let code: CodeFiles = [
        (Digest::of(own.as_bytes()), own.as_bytes().to_vec()),
        (Digest::of(b"fn x() {}"), b"fn x() {}".to_vec()),
        (Digest::of(&[0xff]), vec![0xff]),
    ]
    .into();
    let catalog = temp_env::with_vars(
        [
            ("LEVIATH_HOME", Some(home.path().to_str().unwrap())),
            ("HOME", Some(home.path().to_str().unwrap())),
        ],
        || env.catalog(&code),
    );
    let all = names(&catalog);
    for expected in [
        "read_file",
        "submit_output",
        "spawn_agent",
        "gh__search",
        "global_tool",
        "own_tool",
    ] {
        assert!(all.contains(&expected.to_string()), "{expected} in {all:?}");
    }
    assert!(
        !all.contains(&"orphan".to_string()),
        "a tool with no server is left out"
    );
    assert!(
        !all.contains(&"needy".to_string()),
        "a tool this platform cannot run is left out"
    );
    assert_eq!(all.iter().filter(|n| *n == "read_file").count(), 1);
    let global = catalog
        .iter()
        .find(|t| t.name.as_str() == "global_tool")
        .unwrap();
    assert_eq!(
        global.source,
        ToolSource::Script(Digest::of(b"// @tool global_tool\n1"))
    );

    let mut stage = load_installed_graph().stages[0].clone();
    stage.tools = vec![
        ToolSelector::Tool(ToolName::new("own_tool").unwrap()),
        ToolSelector::Group(ToolGroup::Mcp),
    ];
    let graph = load_installed_graph();
    let picked = env.tools(&graph, &stage, &code).await.unwrap();
    assert_eq!(names(&picked), ["gh__search", "own_tool"]);
}

fn dep(needs: Needs) -> DependencyDef {
    DependencyDef {
        name: "d".into(),
        needs,
        required: true,
        remedy: None,
        description: None,
        install: None,
    }
}

#[tokio::test]
async fn dependencies_are_judged_by_the_daemons_evaluator() {
    let config = Config {
        mcp_servers: vec![leviath_mcp::MCPServerConfig {
            name: "gh".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let (env, _agents) = env_with(config);
    let server = |name: &str| Needs::McpServer {
        server: McpServerName::new(name).unwrap(),
        env: vec![],
    };
    assert!(env.dependency(&dep(server("gh"))).await.is_ok());
    let unmet = env.dependency(&dep(server("jira"))).await.unwrap_err();
    assert!(unmet.contains("configure the MCP server 'jira'"), "{unmet}");
    temp_env::async_with_vars([("LEVIATH_RESOLVE_ENV_VAR", Some("1"))], async {
        assert!(
            env.dependency(&dep(Needs::Env("LEVIATH_RESOLVE_ENV_VAR".into())))
                .await
                .is_ok()
        );
    })
    .await;
    let binary = env
        .dependency(&dep(Needs::Binary("surely-not-installed-anywhere".into())))
        .await
        .unwrap_err();
    assert!(
        binary.contains("install 'surely-not-installed-anywhere'"),
        "{binary}"
    );
    let inline = |src: &str| dep(Needs::Check(CodeRef::Inline(src.into())));
    assert!(env.dependency(&inline("fn check() { () }")).await.is_ok());
    assert_eq!(
        env.dependency(&inline("fn check() { \"get it\" }"))
            .await
            .unwrap_err(),
        "get it"
    );
    let file = env
        .dependency(&dep(Needs::Check(CodeRef::File("c.rhai".into()))))
        .await
        .unwrap_err();
    assert!(file.contains("write the check inline"), "{file}");
}

#[test]
fn providers_and_mcp_servers_are_fingerprinted_from_what_is_configured_now() {
    let mut config = Config::default();
    config.providers.openai_api_key = Some("sk-test".into());
    config.mcp_servers = vec![leviath_mcp::MCPServerConfig {
        name: "quiet".into(),
        ..Default::default()
    }];
    let (env, _agents) = env_with(config.clone());
    let name = |n: &str| ProviderName::new(n).unwrap();
    let creds = crate::commands::run::session::provider_creds_from_config(&config);
    let openai = creds.iter().find(|c| c.name == "openai").unwrap();
    assert_eq!(
        ResolveEnv::provider_fingerprint(&env, &name("openai")),
        Some(host::provider_fingerprint(openai))
    );
    assert_eq!(
        ResolveEnv::provider_fingerprint(&env, &name("mock")),
        Some(host::registered_fingerprint("mock"))
    );
    assert_eq!(ResolveEnv::provider_fingerprint(&env, &name("gone")), None);

    let server = |n: &str| McpServerName::new(n).unwrap();
    let gh = host::mcp_defs(&server("gh"), &[tool("gh__search")]);
    assert_eq!(
        ResolveEnv::mcp_fingerprint(&env, &server("gh")),
        Some(host::tools_fingerprint(&gh))
    );
    assert_eq!(
        ResolveEnv::mcp_fingerprint(&env, &server("quiet")),
        Some(host::tools_fingerprint(&[])),
        "a configured server with no tools yet still exists"
    );
    assert_eq!(ResolveEnv::mcp_fingerprint(&env, &server("gone")), None);
}
