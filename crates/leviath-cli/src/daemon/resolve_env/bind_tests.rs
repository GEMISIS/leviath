use leviath_core::JsonDoc;
use leviath_core::policy::ToolPolicy;
use leviath_runtime::spec::graph::{CodeRef, StageHooks};
use leviath_runtime::spec::launch::{Delivery, LaunchPolicy, Placement};
use leviath_runtime::spec::names::{ModelId, ProfileName, RunId, StageName, ToolName};
use leviath_runtime::spec::run_spec::StagePlan;

use super::*;
use crate::daemon::resolve_env::tests::{MANIFEST, env, install};

/// The test manifest's graph, as a spawn would load it.
fn graph() -> RunGraph {
    let (_, agents) = env();
    install(&agents, "helper", MANIFEST);
    load_installed(Some(agents.path()), &BlueprintRef::parse("helper").unwrap())
        .unwrap()
        .graph
}

/// A resolved run of the test manifest working in `workdir`.
fn spec(workdir: &Path) -> RunSpec {
    let graph = graph();
    let stages = graph
        .stages
        .iter()
        .map(|s| StagePlan {
            stage: s.name.clone(),
            provider: ProviderName::new("mock").unwrap(),
            model: ModelId::new("m").unwrap(),
            context_window: 1000,
            max_output_tokens: None,
            fallbacks: vec![],
            tools: vec![],
            output: None,
            region_budgets: Default::default(),
            notes: vec![],
        })
        .collect();
    RunSpec {
        run_id: RunId::new("bind-test-1").unwrap(),
        origin: SpecOrigin::Blueprint {
            blueprint: BlueprintRef::parse("helper").unwrap(),
            version: "1.2.3".into(),
            manifest: String::new(),
        },
        graph,
        inputs: Default::default(),
        stages,
        seeded: Default::default(),
        code: vec![],
        requested_output: None,
        requested_model: Some(ModelRef::parse("mock/m").unwrap()),
        launch: LaunchPolicy {
            unattended: Unattended::Off,
            allow: vec![ToolName::new("write_file").unwrap()],
            max_depth: 2,
            seed_commands: true,
            capture_model_input: false,
        },
        auto_answers: Default::default(),
        placement: Placement {
            workdir: workdir.to_path_buf(),
            parent: None,
            depth: 0,
            worker_stage: None,
        },
        delivery: Delivery::default(),
        env: Default::default(),
        created_at: 0,
    }
}

/// Place a bound run and hand back its entity.
fn place(spec: RunSpec, bindings: Bindings) -> (bevy_ecs::world::World, bevy_ecs::entity::Entity) {
    let mut world = bevy_ecs::world::World::new();
    let state = leviath_runtime::state::RunState::initial(
        StageName::new("plan").unwrap(),
        Default::default(),
        true,
    );
    let entity = leviath_runtime::insert::insert(&mut world, Arc::new(spec), bindings, &state);
    (world, entity)
}

/// A script tool the run's code holds, given to the `plan` stage.
fn give_script(spec: &mut RunSpec, code: &mut CodeFiles, source: &[u8]) {
    let digest = Digest::of(source);
    code.insert(digest.clone(), source.to_vec());
    spec.stages[0].tools.push(ToolDef {
        name: ToolName::new("own_tool").unwrap(),
        description: "mine".into(),
        schema: JsonDoc::default(),
        source: ToolSource::Script(digest),
    });
}

#[tokio::test]
async fn a_bound_run_is_registered_with_the_tool_service_once_placed() {
    let (env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    let mut s = spec(dir.path());
    let mut code = CodeFiles::new();
    s.stages[0].tools.push(ToolDef {
        name: ToolName::new("read_file").unwrap(),
        description: "read".into(),
        schema: JsonDoc::default(),
        source: ToolSource::Builtin,
    });
    give_script(&mut s, &mut code, b"// @tool own_tool\n1");
    let hook = "fn on_stage_enter(ctx) { () }";
    s.code
        .push((CodeRef::Inline(hook.into()), Digest::of(hook.as_bytes())));
    code.insert(Digest::of(hook.as_bytes()), hook.as_bytes().to_vec());
    s.graph.stages[0].hooks = StageHooks {
        on_stage_enter: Some(CodeRef::Inline(hook.into())),
        ..Default::default()
    };

    let bindings = env.bind(&s, &code).await.unwrap();
    assert_eq!(
        bindings.len(),
        5,
        "the compiled hooks, the mime registry, the title chain, the record and the registration"
    );
    let (world, entity) = place(s, bindings);
    assert!(
        world
            .get::<leviath_runtime::components::StageHookScripts>(entity)
            .is_some()
    );
    let state = env.tool_service.state_for(entity).expect("registered");
    assert!(state.script_tool_names.lock().unwrap().contains("own_tool"));
    assert_eq!(
        state.launch_overrides.get("write_file"),
        Some(&ToolPolicy::Allow)
    );
    assert_eq!(state.agent_perms.len(), 0);
}

#[tokio::test]
async fn an_unattended_run_with_its_own_entry_sandbox_and_grants_binds() {
    let _trace = leviath_testkit::tracing_guard();
    let (env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    let mut s = spec(dir.path());
    s.launch.unattended = Unattended::All;
    s.graph.entry = Some(StageName::new("work").unwrap());
    s.graph.sandbox = Some(leviath_runtime::spec::graph::SandboxDef {
        kind: leviath_core::sandbox::SandboxKind::Namespace,
        image: None,
        engine: None,
        network: true,
        mounts: vec![],
        keep_warm: false,
        on_unavailable: leviath_core::sandbox::OnUnavailable::Warn,
    });
    s.graph.read_paths = vec!["~/notes".into()];
    s.graph.safe_commands.shell = vec!["git status".into()];
    s.graph.tool_permissions = [
        (ToolName::new("read_file").unwrap(), ToolPolicy::Allow),
        (ToolName::new("shell").unwrap(), ToolPolicy::Ask),
        (ToolName::new("web_fetch").unwrap(), ToolPolicy::Deny),
    ]
    .into();
    s.graph.stages[1].required_tools = vec![ToolName::new("bash").unwrap()];
    s.graph.stages[1].tool_accepts = [(
        ToolName::new("read_file").unwrap(),
        vec![leviath_runtime::spec::names::MimePattern::new("text/*").unwrap()],
    )]
    .into();
    let bindings = env.bind(&s, &CodeFiles::new()).await.unwrap();
    let (_world, entity) = place(s, bindings);
    let state = env.tool_service.state_for(entity).expect("registered");
    assert_eq!(
        state.agent_perms.get("web_fetch").map(String::as_str),
        Some("deny")
    );
    assert_eq!(
        state.agent_perms.get("shell").map(String::as_str),
        Some("ask")
    );
    assert!(state.stage_required_by_index[1].contains("shell"));
    assert_eq!(
        state.stage_tool_accepts_by_index[1].get("read_file"),
        Some(&vec!["text/*".to_string()])
    );
}

#[tokio::test]
async fn everything_that_stops_a_run_binding_is_reported_at_once() {
    let home = tempfile::tempdir().unwrap();
    let (env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    let mut s = spec(dir.path());
    s.launch.unattended = Unattended::Profile(ProfileName::new("no-such-profile").unwrap());
    s.graph.sandbox = Some(leviath_runtime::spec::graph::SandboxDef {
        kind: leviath_core::sandbox::SandboxKind::Container,
        image: None,
        engine: Some("docker".into()),
        network: true,
        mounts: vec![],
        keep_warm: false,
        on_unavailable: leviath_core::sandbox::OnUnavailable::Error,
    });
    s.graph.read_paths = vec!["regex:".into()];
    let mut code = CodeFiles::new();
    give_script(&mut s, &mut code, &[0xff]);
    let mut no_text = spec(dir.path());
    give_script(&mut no_text, &mut CodeFiles::new(), b"absent");
    let issues = temp_env::async_with_vars(
        [("LEVIATH_HOME", Some(home.path().to_str().unwrap()))],
        env.bind(&s, &code),
    )
    .await
    .unwrap_err();
    let paths: Vec<String> = issues.iter().map(|i| i.path.to_string()).collect();
    assert_eq!(
        paths,
        ["launch", "sandbox", "read_paths", "stages.plan.tools"]
    );
    assert!(issues.0[3].message.contains("own_tool"), "{}", issues.0[3]);

    let mut bad_script = spec(dir.path());
    let mut code = CodeFiles::new();
    give_script(&mut bad_script, &mut code, b"// @tool own_tool\nlet");
    let issues = env.bind(&bad_script, &code).await.unwrap_err();
    assert_eq!(issues.0[0].path.to_string(), "stages.plan.tools");
    let issues = env.bind(&no_text, &CodeFiles::new()).await.unwrap_err();
    assert!(issues.0[0].message.contains("no text"), "{}", issues.0[0]);

    let mut broken_hook = spec(dir.path());
    broken_hook.graph.stages[0].hooks.on_stage_enter = Some(CodeRef::File("h.rhai".into()));
    let issues = env.bind(&broken_hook, &CodeFiles::new()).await.unwrap_err();
    assert_eq!(
        issues.0[0].path.to_string(),
        "stages.plan.hooks.on_stage_enter"
    );
}

#[test]
fn a_run_is_named_by_its_blueprint_or_its_title() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = spec(dir.path());
    assert_eq!(bind::agent_name(&s), "helper");
    s.origin = SpecOrigin::Raw;
    s.graph.title = Some("my run".into());
    assert_eq!(bind::agent_name(&s), "my run");
    s.graph.title = None;
    assert_eq!(bind::agent_name(&s), "raw");
}

#[tokio::test]
async fn a_bound_run_carries_what_only_the_daemon_knows() {
    let mut config = Config::default();
    config.title.enabled = false;
    config.security.read_paths = vec!["~/notes".into()];
    let (env, _agents) = crate::daemon::resolve_env::tests::env_with(config);
    let dir = tempfile::tempdir().unwrap();
    let mut s = spec(dir.path());
    s.graph.taint_tracking = Some(true);
    s.graph.tool_rescan = leviath_runtime::spec::graph::ToolRescan::AfterWrites;
    s.graph.read_paths = vec!["~/notes".into()];
    s.stages[0].fallbacks = vec![
        ModelRef::parse("mock/backup").unwrap(),
        ModelRef::parse("bare-model").unwrap(),
    ];
    s.stages[0].tools.push(ToolDef {
        name: ToolName::new("read_file").unwrap(),
        description: "read".into(),
        schema: JsonDoc::default(),
        source: ToolSource::Builtin,
    });
    let bindings = env.bind(&s, &CodeFiles::new()).await.unwrap();
    let (world, entity) = place(s.clone(), bindings);
    assert!(world.get::<leviath_runtime::TaintGate>(entity).is_some());
    let levels = &world
        .get::<leviath_runtime::pipeline::ToolSensitivities>(entity)
        .unwrap()
        .0;
    assert_eq!(
        levels.get("read_file"),
        Some(&leviath_core::TaintLevel::Private),
        "a run that may read outside its workdir reads private things"
    );
    assert!(
        world
            .get::<leviath_runtime::title::TitleCandidates>(entity)
            .is_none(),
        "titles are off"
    );
    let meta = world
        .get::<leviath_runtime::persistence::RunMetadata>(entity)
        .unwrap();
    assert!(
        meta.agent_path.ends_with("agent.toml"),
        "{}",
        meta.agent_path
    );
    let counts = meta.read_paths.expect("the run declares read paths");
    assert_eq!((counts.declared, counts.granted), (1, 1));
    let state = env.tool_service.state_for(entity).unwrap();
    let rescan = state.dynamic.as_ref().expect("the run rescans its tools");
    assert!(rescan.scan_dirs.iter().any(|d| d.ends_with("helper/tools")));
    assert_eq!(rescan.stage_available.len(), 2);

    let mut raw = s;
    raw.origin = SpecOrigin::Raw;
    raw.graph.taint_tracking = Some(false);
    raw.graph.read_paths.clear();
    let bindings = env.bind(&raw, &CodeFiles::new()).await.unwrap();
    let (world, entity) = place(raw, bindings);
    assert!(world.get::<leviath_runtime::TaintGate>(entity).is_none());
    let meta = world
        .get::<leviath_runtime::persistence::RunMetadata>(entity)
        .unwrap();
    assert_eq!(meta.agent_path, "", "a raw graph has no blueprint on disk");
    assert!(meta.read_paths.is_none());
}

#[tokio::test]
async fn mime_rows_that_will_not_build_are_reported_beside_the_tool_states_problems() {
    let (env, _agents) = env();
    let dir = tempfile::tempdir().unwrap();
    let mut s = spec(dir.path());
    s.graph.mime_types.insert(
        leviath_runtime::spec::names::MimePattern::new("application/x-odd").unwrap(),
        leviath_runtime::spec::graph::MimeRowDef {
            magic: Some("not hex".into()),
            ..Default::default()
        },
    );
    let paths = |issues: SpawnIssues| -> Vec<String> {
        issues.iter().map(|i| i.path.to_string()).collect()
    };
    let alone = env.bind(&s, &CodeFiles::new()).await.unwrap_err();
    assert_eq!(paths(alone), ["mime_types"]);
    s.graph.read_paths = vec!["regex:".into()];
    let both = env.bind(&s, &CodeFiles::new()).await.unwrap_err();
    assert_eq!(paths(both), ["mime_types", "read_paths"]);
}

#[test]
fn the_daemon_lists_an_mcp_servers_tools_for_a_changed_fingerprint() {
    let (env, _agents) = env();
    let gh = McpServerName::new("gh").unwrap();
    let tools = BindEnv::mcp_tools(&env, &gh).unwrap();
    assert_eq!(tools[0].name.as_str(), "gh__search");
    assert_eq!(
        BindEnv::mcp_fingerprint(&env, &gh),
        Some(host::tools_fingerprint(&tools))
    );
    assert_eq!(
        BindEnv::provider_fingerprint(&env, &ProviderName::new("mock").unwrap()),
        Some(host::registered_fingerprint("mock"))
    );
}

/// What `lev validate` would warn about a run's blueprint is logged against
/// the run; a run with no blueprint, or one that will not read, logs nothing.
#[test]
fn a_blueprints_lint_findings_are_logged_against_the_run() {
    assert!(lint_lines(None).is_empty());
    let empty = tempfile::tempdir().unwrap();
    assert!(lint_lines(Some(empty.path())).is_empty());
    let (env, agents) = env();
    let manifest = install(
        &agents,
        "helper",
        &MANIFEST.replace("tools = [\"read_file\"]", "tools = [\"raed_file\"]"),
    );
    let lines = lint_lines(manifest.parent());
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("blueprint 'helper': stage 'plan'")
                && l.ends_with("[unknown-tool]")),
        "{lines:#?}"
    );
    env.log_lint(&spec(empty.path()));
}

/// What a refusal to resume lists as known is what this machine has now:
/// the providers in its config and registry, and the MCP servers it has
/// configured or connected.
#[test]
fn the_daemon_names_the_providers_and_servers_it_has_now() {
    let mut config = crate::config::Config::default();
    config.providers.openai_api_key = Some("sk-test".into());
    config.mcp_servers = vec![leviath_mcp::MCPServerConfig {
        name: "quiet".into(),
        ..Default::default()
    }];
    let (env, _agents) = crate::daemon::resolve_env::tests::env_with(config);
    assert_eq!(BindEnv::providers_now(&env), ["mock", "openai"]);
    assert_eq!(BindEnv::mcp_servers_now(&env), ["gh", "quiet"]);
}
