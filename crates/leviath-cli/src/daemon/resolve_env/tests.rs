use super::*;
use crate::test_support::FakeProvider;
use leviath_runtime::spec::graph::{ToolGroup, ToolSelector};
use leviath_runtime::spec::names::ToolName;

/// A small valid manifest: two stages, one without an iteration ceiling.
pub(crate) const MANIFEST: &str = r#"[blueprint]
name = "helper"
version = "1.2.3"

[graph]
edges = [{ name = "work", from = "plan", to = "work" }]

[[graph.stages]]
name = "plan"
model = { models = [{ provider = "mock", model = "m" }] }
tools = ["read_file"]
max_iterations = 5

[[graph.stages]]
name = "work"
model = { models = [{ provider = "mock", model = "m" }] }
tools = [
    "read_file",
    "write_file",
]
max_iterations = 0

[graph.layout]
regions = [
    { name = "system", kind = "pinned", budget = 1000 },
    { name = "task", kind = "pinned", budget = 1000 },
]
total_budget_tokens = 2000
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
        blob_store: Arc::new(leviath_core::mime::MemoryBlobStore::new()),
        mcp_overrides: HashMap::new(),
    };
    (env, agents)
}

pub(crate) fn env() -> (DaemonEnv, tempfile::TempDir) {
    env_with(Config::default())
}

/// Install `manifest` as `name` under the env's agents directory, renamed
/// to match: an installed blueprint is named after itself.
pub(crate) fn install(agents: &tempfile::TempDir, name: &str, manifest: &str) -> PathBuf {
    let dir = agents.path().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let named = manifest.replace("name = \"helper\"", &format!("name = \"{name}\""));
    crate::test_support::write_test_agent(&dir, named)
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
        [Some(5), Some(0)],
        "the operator's ceiling is folded in by resolution, for every graph"
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
    assert_eq!(moved.code, IssueCode::Changed);
    assert_eq!(moved.path.to_string(), "digest");
    assert_eq!(moved.got, Some(format!("helper@{digest}")));

    let with_servers = MANIFEST.replace(
        "[graph]\n",
        "[graph]\nmcp_servers = [{ name = \"own\", command = \"own-server\" }]\n\
         script_permissions = { shell = \"deny\" }\n",
    );
    install(&agents, "served", &with_servers);
    let served = env.blueprint(&reference("served")).await.unwrap();
    assert_eq!(served.graph.mcp_servers[0].name.as_str(), "own");
    assert!(served.graph.script_permissions.shell.is_some());
    let bad = MANIFEST.replace("[graph]\n", "[graph]\nmcp_servers = [{ name = 3 }]\n");
    install(&agents, "bad-servers", &bad);
    let refused = env.blueprint(&reference("bad-servers")).await.unwrap_err();
    assert!(refused.message.contains("mcp_servers"), "{refused}");
}

#[test]
fn the_operators_defaults_are_handed_to_resolution() {
    let mut config = Config::default();
    config.limits.default_max_iterations = Some(7);
    config.batch_tool_hint = false;
    config.taint_tracking = true;
    config.observability.capture_model_input = true;
    config.nudge.max = Some(2);
    config.nudge.text = Some("keep going".into());
    let (env, _agents) = env_with(config);
    let limits = env.limits();
    assert_eq!(limits.default_max_iterations, Some(7));
    let d = &limits.defaults;
    assert!(!d.batch_tool_hint && d.shell_hint);
    assert!(d.taint_tracking && d.capture_model_input);
    assert_eq!(d.nudge.max, Some(2));
    assert_eq!(d.nudge.text.as_deref(), Some("keep going"));
}

#[test]
fn unattended_settings_answer_by_the_yolo_profile() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".leviath")).unwrap();
    std::fs::write(
        home.path().join(".leviath").join("yolo.toml"),
        "[careful]\ndefault = \"allow\"\nquestions = \"ask\"\ncheckpoints = \"ask\"\ngate = \"auto\"\n",
    )
    .unwrap();
    let (env, _agents) = env();
    let profile = |name: &str| {
        Unattended::Profile(leviath_runtime::spec::names::ProfileName::new(name).unwrap())
    };
    let at = |dir: &Path| {
        [
            ("LEVIATH_HOME", Some(dir.to_path_buf().into_os_string())),
            ("LEVIATH_CONFIG_PATH", None),
        ]
    };
    temp_env::with_vars(at(home.path()), || {
        assert_eq!(
            env.auto_answers(&Unattended::Off),
            Ok(AutoAnswers::default())
        );
        assert_eq!(env.auto_answers(&Unattended::All), Ok(AutoAnswers::all()));
        assert_eq!(
            env.auto_answers(&profile("careful")),
            Ok(AutoAnswers {
                questions: false,
                checkpoints: false,
                gate: true,
            })
        );
        let missing = env.auto_answers(&profile("nope")).unwrap_err();
        assert_eq!(missing.code, IssueCode::Unresolvable);
        assert_eq!(missing.known, ["careful"]);
    });
    let empty = tempfile::tempdir().unwrap();
    temp_env::with_vars(at(empty.path()), || {
        let no_file = env.auto_answers(&profile("careful")).unwrap_err();
        assert!(no_file.known.is_empty());
        assert!(no_file.message.contains("does not exist"), "{no_file}");
    });
}

#[test]
fn a_compaction_model_answers_to_the_operators_retention_rules() {
    let (env, _agents) = env();
    assert!(
        env.compaction_model(&ModelRef::parse("mock/m").unwrap())
            .is_ok()
    );
}

#[tokio::test]
async fn a_blueprint_that_is_missing_or_will_not_load_is_an_issue() {
    let (env, agents) = env();
    install(&agents, "helper", MANIFEST);
    std::fs::create_dir_all(agents.path().join("empty-dir")).unwrap();
    let missing = env.blueprint(&reference("nope")).await.unwrap_err();
    // Relative to the source; the resolver adds `source.blueprint`.
    assert_eq!(missing.path.to_string(), "(request)");
    assert_eq!(missing.known, ["helper"]);
    let request = leviath_runtime::spec::request::SpawnRequest::new(
        leviath_runtime::spec::request::SpawnSource::Blueprint(reference("nope")),
    );
    let issues = leviath_runtime::resolve::resolve(
        &request,
        &leviath_runtime::spec::env::Caller::TopLevel,
        &env,
        leviath_runtime::resolve::ResolveMode::Check,
    )
    .await
    .unwrap_err();
    let paths: Vec<String> = issues.iter().map(|i| i.path.to_string()).collect();
    assert!(paths.contains(&"source.blueprint".to_string()), "{paths:?}");

    install(&agents, "broken", "this is not toml [");
    let broken = env.blueprint(&reference("broken")).await.unwrap_err();
    assert_eq!(broken.code, IssueCode::Invalid);
    assert!(
        broken.message.contains("is not a valid blueprint"),
        "{broken}"
    );
    assert!(broken.hint.is_some());

    // A tool name that is not a name is refused as the file is read.
    install(
        &agents,
        "odd",
        &MANIFEST.replace("tools = [\"read_file\"]", "tools = [\"no spaces allowed\"]"),
    );
    let odd = env.blueprint(&reference("odd")).await.unwrap_err();
    assert!(odd.message.contains("is not a valid blueprint"), "{odd}");

    // A graph that reads but does not hold together loads; resolving it is
    // what refuses it, with every problem.
    install(
        &agents,
        "invalid",
        &MANIFEST.replace("[graph]\n", "[graph]\nentry = \"ghost\"\n"),
    );
    assert!(env.blueprint(&reference("invalid")).await.is_ok());

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
    let registry = env.mime_registry(&MimeRows::new()).unwrap();
    assert_eq!(
        env.sniff(&registry, "a.txt", b"hi", None).unwrap(),
        "text/plain"
    );
}

/// The test manifest's graph, read the way a spawn reads it.
pub(crate) fn load_installed_graph() -> RunGraph {
    let (_, agents) = env();
    install(&agents, "helper", MANIFEST);
    load_installed(Some(agents.path()), &reference("helper"))
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
        || env.catalog(&code, None).defs,
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
    let picked = env.tools(&graph, &stage, &code, None).await.unwrap();
    assert_eq!(names(&picked.tools), ["gh__search", "own_tool"]);
    assert!(picked.code.is_empty(), "the run already holds its own tool");
}

#[tokio::test]
async fn script_tools_found_on_disk_come_back_with_their_code() {
    let home = tempfile::tempdir().unwrap();
    let global_dir = home.path().join(".leviath").join("tools");
    std::fs::create_dir_all(&global_dir).unwrap();
    std::fs::write(global_dir.join("global.rhai"), "// @tool global_tool\n1").unwrap();
    std::fs::write(global_dir.join("both.rhai"), "// @tool both\n\"global\"").unwrap();
    let blueprint = tempfile::tempdir().unwrap();
    let own_dir = blueprint.path().join("tools");
    std::fs::create_dir_all(&own_dir).unwrap();
    std::fs::write(own_dir.join("mine.rhai"), "// @tool mine\n2").unwrap();
    std::fs::write(own_dir.join("both.rhai"), "// @tool both\n\"own\"").unwrap();
    let (env, _agents) = env();
    let mut stage = load_installed_graph().stages[0].clone();
    stage.tools = vec![ToolSelector::Group(ToolGroup::Scripts)];
    let graph = load_installed_graph();
    let picked = temp_env::async_with_vars(
        [("LEVIATH_HOME", Some(home.path().to_str().unwrap()))],
        env.tools(&graph, &stage, &CodeFiles::new(), Some(blueprint.path())),
    )
    .await
    .unwrap();
    assert_eq!(names(&picked.tools), ["both", "mine", "global_tool"]);
    let found: BTreeMap<String, Vec<u8>> = picked
        .code
        .into_iter()
        .map(|(reference, bytes)| match reference {
            CodeRef::File(path) => (path, bytes),
            CodeRef::Inline(_) => panic!("found code is named by where it was read"),
        })
        .collect();
    assert_eq!(found["tools/mine.rhai"], b"// @tool mine\n2");
    assert_eq!(
        found["tools/both.rhai"], b"// @tool both\n\"own\"",
        "the blueprint's own wins a name"
    );
    let global = found
        .iter()
        .find(|(path, _)| path.ends_with("global.rhai"))
        .unwrap();
    assert!(Path::new(global.0).is_absolute(), "{}", global.0);
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
    assert!(env.dependency(&dep(server("gh")), None).await.is_ok());
    let unmet = env
        .dependency(&dep(server("jira")), None)
        .await
        .unwrap_err();
    assert!(unmet.contains("configure the MCP server 'jira'"), "{unmet}");
    temp_env::async_with_vars([("LEVIATH_RESOLVE_ENV_VAR", Some("1"))], async {
        let set = dep(Needs::Env("LEVIATH_RESOLVE_ENV_VAR".into()));
        assert!(env.dependency(&set, None).await.is_ok());
    })
    .await;
    let missing = dep(Needs::Binary("surely-not-installed-anywhere".into()));
    let binary = env.dependency(&missing, None).await.unwrap_err();
    assert!(
        binary.contains("install 'surely-not-installed-anywhere'"),
        "{binary}"
    );
    let by_file = dep(Needs::Check(CodeRef::File("c.rhai".into())));
    let ok: &[u8] = b"fn check() { () }";
    assert!(env.dependency(&by_file, Some(ok)).await.is_ok());
    let unmet: &[u8] = b"fn check() { \"get it\" }";
    assert_eq!(
        env.dependency(&by_file, Some(unmet)).await.unwrap_err(),
        "get it"
    );
    let none = env.dependency(&by_file, None).await.unwrap_err();
    assert!(none.contains("holds no code"), "{none}");
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

/// A blueprint read from a directory whose manifest gives it a name no
/// blueprint may have is refused, naming both.
#[test]
fn a_blueprint_file_with_a_name_too_long_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let long = "x".repeat(200);
    crate::daemon::starter::testing::manifest_in(
        dir.path(),
        &format!(
            r#"[blueprint]
name = "{long}"
version = "0.1.0"

[[graph.stages]]
name = "main"
model = {{ models = [{{ provider = "anthropic", model = "claude-sonnet-4-6" }}] }}

[graph.layout]
total_budget_tokens = 18000

[[graph.layout.regions]]
name = "system"
kind = "pinned"
budget = 8000

[[graph.layout.regions]]
name = "conversation"
kind = {{ kind = "sliding_window", max_items = 10 }}
budget = 10000
"#
        ),
    );
    let path =
        leviath_runtime::spec::names::BlueprintPath::new(dir.path().to_string_lossy()).unwrap();
    let issue = load_file(&path).unwrap_err();
    assert!(issue.message.contains(&long), "{}", issue.message);
}
