use super::*;
use crate::bind::host::tests::{registry, stage};
use crate::spec::graph::{SeedToolCall, ToolSelector};
use crate::spec::inputs::InputValues;
use crate::spec::names::ToolName;

fn env() -> EmbedEnv {
    EmbedEnv::new(registry(&["mock"]), ModelDefaults::default())
}

fn loaded(digest: Option<Digest>) -> LoadedBlueprint {
    LoadedBlueprint {
        graph: crate::spec::graph::tests::minimal(),
        reference: BlueprintRef {
            name: crate::spec::names::BlueprintName::new("coder").unwrap(),
            digest,
        },
        version: "1.0.0".into(),
        base_dir: PathBuf::from("/nowhere"),
    }
}

fn reference(text: &str) -> BlueprintRef {
    BlueprintRef::parse(text).unwrap()
}

#[tokio::test]
async fn registered_blueprints_load_by_name_and_revision() {
    let d = Digest::of(b"one");
    let env = env().with_blueprint(loaded(Some(d.clone())));
    assert_eq!(
        env.blueprint(&reference("coder")).await.unwrap().version,
        "1.0.0"
    );
    assert!(
        env.blueprint(&reference(&format!("coder@{d}")))
            .await
            .is_ok()
    );

    let unknown = env.blueprint(&reference("nope")).await.unwrap_err();
    assert_eq!(unknown.path.to_string(), "source.blueprint");
    assert_eq!(unknown.known, ["coder"]);

    let other = Digest::of(b"two");
    let moved = env
        .blueprint(&reference(&format!("coder@{other}")))
        .await
        .unwrap_err();
    assert_eq!(moved.got, Some(format!("revision {d}")));
    let unpinned = EmbedEnv::new(registry(&[]), ModelDefaults::default())
        .with_blueprint(loaded(None))
        .blueprint(&reference(&format!("coder@{other}")))
        .await
        .unwrap_err();
    assert_eq!(unpinned.got.as_deref(), Some("no recorded revision"));
}

#[test]
fn limits_default_to_a_cautious_embedder_and_can_be_set() {
    let limits = env().limits();
    assert_eq!(limits.default_max_depth, 3);
    assert!(!limits.seed_commands_allowed);
    assert_eq!(limits.default_max_iterations, None);
    assert_eq!(limits.defaults, Default::default());
    let set = SpawnLimits {
        default_max_depth: 1,
        seed_commands_allowed: true,
        max_attachment_bytes: 9,
        default_max_iterations: Some(4),
        defaults: Default::default(),
    };
    assert_eq!(env().with_limits(set.clone()).limits(), set);
}

#[test]
fn an_embedded_world_has_no_yolo_profiles() {
    use crate::spec::launch::Unattended;
    use crate::spec::run_spec::AutoAnswers;
    let e = env();
    assert_eq!(e.auto_answers(&Unattended::Off), Ok(AutoAnswers::default()));
    assert_eq!(e.auto_answers(&Unattended::All), Ok(AutoAnswers::all()));
    let named = Unattended::Profile(crate::spec::names::ProfileName::new("safe").unwrap());
    let refused = e.auto_answers(&named).unwrap_err();
    assert_eq!(refused.code, IssueCode::Unresolvable);
    assert!(refused.message.contains("\"safe\""), "{refused}");
}

#[test]
fn a_compaction_model_is_judged_by_the_operators_retention_rules() {
    let model = |m: &str| ModelRef::parse(m).unwrap();
    assert!(env().compaction_model(&model("mock/m")).is_ok());
    assert!(
        env().compaction_model(&model("gone/m")).is_ok(),
        "never called"
    );
    assert!(
        env().compaction_model(&model("m")).is_ok(),
        "no provider named"
    );
}

#[test]
fn run_ids_are_minted_from_a_short_safe_stem() {
    let id = env().new_run_id(&"A Very Long Title ".repeat(20));
    assert!(id.as_str().starts_with("a-very-long-title"), "{id}");
    assert!(id.as_str().len() < 80, "{id}");
    assert!(env().new_run_id("").as_str().starts_with("agent-"));
}

#[test]
fn a_workdir_must_be_an_existing_directory() {
    let dir = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    assert_eq!(env().workdir(Some(dir.path())).unwrap(), canonical);
    let with_default = env().with_default_workdir(dir.path());
    assert_eq!(with_default.workdir(None).unwrap(), canonical);
    let none = env().workdir(None).unwrap_err();
    assert!(none.contains("names its workdir"), "{none}");
    let missing = env().workdir(Some(&dir.path().join("gone"))).unwrap_err();
    assert!(missing.contains("does not exist"), "{missing}");
}

#[test]
fn paths_are_checked_inside_the_workdir_by_kind() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "").unwrap();
    let e = env();
    let p = |s: &str| WorkdirPath::new(s).unwrap();
    assert!(e.path_exists(dir.path(), &p("src/a.rs"), PathKind::File));
    assert!(!e.path_exists(dir.path(), &p("src"), PathKind::File));
    assert!(e.path_exists(dir.path(), &p("src"), PathKind::Dir));
    assert!(!e.path_exists(dir.path(), &p("src/a.rs"), PathKind::Dir));
    assert!(e.path_exists(dir.path(), &p("src"), PathKind::Any));
    assert!(!e.path_exists(dir.path(), &p("gone"), PathKind::Any));
}

#[tokio::test]
async fn models_tools_and_code_answer_through_the_shared_host() {
    let e = env();
    let plan = e.model(&stage(&["mock/m"]), None).await.unwrap();
    assert_eq!(plan.provider.as_str(), "mock");
    let mut nowhere = stage(&["gone/x"]);
    nowhere.model.allow_user_default = false;
    let issue = e.model(&nowhere, None).await.unwrap_err();
    assert_eq!(issue.path.to_string(), "stages.plan.model");

    let mut s = stage(&[]);
    s.tools = vec![ToolSelector::Tool(ToolName::new("read_file").unwrap())];
    let graph = crate::spec::graph::tests::minimal();
    let tools = e.tools(&graph, &s, &CodeFiles::new(), None).await.unwrap();
    assert_eq!(tools.tools.len(), 1);
    assert_eq!(tools.tools[0].name.as_str(), "read_file");
    assert!(tools.code.is_empty());

    assert_eq!(
        e.code(&CodeRef::Inline("x".into()), None).await.unwrap(),
        b"x"
    );
    assert!(
        e.check_code(b"fn check() { () }", CodeUse::DependencyCheck)
            .is_ok()
    );
    let rows = MimeRows::new();
    let registry = e.mime_registry(&rows).unwrap();
    assert_eq!(
        e.sniff(&registry, "a.txt", b"hi", None).unwrap(),
        "text/plain"
    );
    let custom = env().with_mime_registry(leviath_core::mime::MimeRegistry::empty());
    let registry = custom.mime_registry(&rows).unwrap();
    assert_eq!(
        custom.sniff(&registry, "x", &[0xff], None).unwrap(),
        "application/octet-stream"
    );
}

#[tokio::test]
async fn only_literal_seeds_run_in_an_embedded_world() {
    let dir = tempfile::tempdir().unwrap();
    let code = CodeFiles::new();
    let inputs = InputValues::default();
    let spec = crate::spec::run_spec::tests::spec();
    let cx = SeedCx {
        run_id: &spec.run_id,
        agent: "coder",
        graph: &spec.graph,
        launch: &spec.launch,
        workdir: dir.path(),
        commands_allowed: true,
        code: &code,
        code_refs: &[],
        inputs: &inputs,
    };
    let e = env();
    let text = e.seed(&Seed::Literal("hello".into()), cx).await.unwrap();
    assert_eq!(text.text, "hello");
    let refused = [
        (Seed::Glob("*.md".into()), "glob"),
        (Seed::Files(vec![WorkdirPath::new("a").unwrap()]), "files"),
        (Seed::Code(CodeRef::Inline("1".into())), "code"),
        (Seed::Command("ls".into()), "command"),
        (
            Seed::Tools {
                calls: vec![SeedToolCall {
                    tool: ToolName::new("read_file").unwrap(),
                    args: Default::default(),
                }],
                refresh: Default::default(),
            },
            "tool",
        ),
    ];
    for (seed, kind) in refused {
        let err = e.seed(&seed, cx).await.unwrap_err();
        assert!(err.contains(&format!("does not run {kind} seeds")), "{err}");
    }
}

fn dep(needs: Needs, remedy: Option<&str>) -> DependencyDef {
    DependencyDef {
        name: "d".into(),
        needs,
        required: true,
        remedy: remedy.map(str::to_string),
        description: None,
        install: None,
    }
}

#[tokio::test]
async fn dependencies_on_the_environment_and_path_are_checked() {
    let e = env();
    temp_env::async_with_vars(
        [
            ("LEVIATH_EMBED_ENV_SET", Some("1")),
            ("LEVIATH_EMBED_ENV_UNSET", None),
        ],
        async {
            assert!(
                e.dependency(&dep(Needs::Env("LEVIATH_EMBED_ENV_SET".into()), None), None)
                    .await
                    .is_ok()
            );
            let unset = e
                .dependency(
                    &dep(Needs::Env("LEVIATH_EMBED_ENV_UNSET".into()), None),
                    None,
                )
                .await
                .unwrap_err();
            assert!(unset.contains("set the environment variable"), "{unset}");
        },
    )
    .await;

    let bin = tempfile::tempdir().unwrap();
    std::fs::write(bin.path().join("present-tool"), "").unwrap();
    std::fs::write(bin.path().join("windows-tool.exe"), "").unwrap();
    let path = bin.path().to_string_lossy().into_owned();
    temp_env::async_with_vars([("PATH", Some(path.as_str()))], async {
        for program in ["present-tool", "windows-tool"] {
            assert!(
                e.dependency(&dep(Needs::Binary(program.into()), None), None)
                    .await
                    .is_ok()
            );
        }
        let absent = e
            .dependency(
                &dep(Needs::Binary("absent-tool".into()), Some("brew it")),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(absent, "brew it");
    })
    .await;
    assert!(!on_path(None, "anything"));
}

#[tokio::test]
async fn mcp_dependencies_are_never_met_and_checks_run_from_the_runs_code() {
    let e = env();
    let server = Needs::McpServer {
        server: McpServerName::new("gh").unwrap(),
        env: vec![],
    };
    let mcp = e.dependency(&dep(server, None), None).await.unwrap_err();
    assert!(mcp.contains("connects none"), "{mcp}");
    let by_file = dep(Needs::Check(CodeRef::File("c.rhai".into())), None);
    let check = |src: &str| {
        let e = &e;
        let by_file = by_file.clone();
        let src = src.to_string();
        async move { e.dependency(&by_file, Some(src.as_bytes())).await }
    };
    assert!(check("fn check() { () }").await.is_ok());
    assert_eq!(
        check("fn check() { \"install it\" }").await.unwrap_err(),
        "install it"
    );
    let thrown = check("fn check() { throw \"boom\" }").await.unwrap_err();
    assert!(thrown.contains("boom"), "{thrown}");
    let broken = check("fn nothing() {}").await.unwrap_err();
    assert!(broken.contains("check()"), "{broken}");
    let missing = e.dependency(&by_file, None).await.unwrap_err();
    assert!(missing.contains("holds no code"), "{missing}");
    let binary = e.dependency(&by_file, Some(&[0xff])).await.unwrap_err();
    assert!(binary.contains("not UTF-8"), "{binary}");
}

#[test]
fn providers_are_fingerprinted_by_their_credentials_or_their_name() {
    let mut creds = ProviderCreds::simple("keyed");
    creds.base_url = Some("https://x.example".into());
    let e = EmbedEnv::new(registry(&["keyed", "custom"]), ModelDefaults::default())
        .with_creds(vec![creds.clone()]);
    let name = |n: &str| ProviderName::new(n).unwrap();
    assert_eq!(
        ResolveEnv::provider_fingerprint(&e, &name("keyed")),
        Some(host::provider_fingerprint(&creds))
    );
    assert_eq!(
        BindEnv::provider_fingerprint(&e, &name("custom")),
        Some(host::registered_fingerprint("custom"))
    );
    assert_eq!(ResolveEnv::provider_fingerprint(&e, &name("gone")), None);
    let gh = McpServerName::new("gh").unwrap();
    assert_eq!(ResolveEnv::mcp_fingerprint(&e, &gh), None);
    assert_eq!(BindEnv::mcp_fingerprint(&e, &gh), None);
}

#[tokio::test]
async fn binding_registers_the_run_with_the_basic_tool_service() {
    use crate::pipeline::ToolService;
    let mut spec = crate::spec::run_spec::tests::spec();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), "from the workdir").unwrap();
    spec.placement.workdir = dir.path().to_path_buf();

    assert_eq!(
        env().bind(&spec, &CodeFiles::new()).await.unwrap().len(),
        1,
        "the run's mime registry alone"
    );
    let mut hooked = spec.clone();
    hooked.graph.stages[0].hooks.on_stage_enter = Some(CodeRef::File("hooks/enter.rhai".into()));
    let issues = env().bind(&hooked, &CodeFiles::new()).await.unwrap_err();
    assert_eq!(
        issues.0[0].path.to_string(),
        "stages.plan.hooks.on_stage_enter"
    );

    let tools = Arc::new(BasicToolService::new(
        crate::interaction_hub::InteractionHub::new(),
    ));
    let bindings = env()
        .with_basic_tools(tools.clone())
        .bind(&spec, &CodeFiles::new())
        .await
        .unwrap();
    let mut world = bevy_ecs::world::World::new();
    let entity = crate::insert::insert(
        &mut world,
        Arc::new(spec),
        bindings,
        &crate::state::RunState::initial(
            crate::spec::names::StageName::new("plan").unwrap(),
            Default::default(),
            true,
        ),
    );
    let call = leviath_providers::ToolCall {
        id: "c1".into(),
        name: "read_file".into(),
        arguments: serde_json::json!({"path": "note.txt"}),
        thought_signature: None,
    };
    let results = tools.exec_for(entity, vec![call], crate::pipeline::noop_progress())().await;
    assert!(
        results[0].1.contains("from the workdir"),
        "{:?}",
        results[0].1
    );
}

/// An embedded host reads no blueprint from a path, and says so.
#[tokio::test]
async fn an_embedded_host_reads_no_blueprint_from_a_path() {
    let path = crate::spec::names::BlueprintPath::new(
        std::env::temp_dir().join("coder").to_string_lossy(),
    )
    .unwrap();
    let issue = env().blueprint_file(&path).await.unwrap_err();
    assert_eq!(issue.code, IssueCode::NotAllowed);
    assert!(issue.message.contains("coder"), "{}", issue.message);
}

/// A manifest that parses and validates but does not read as a graph, or
/// whose name cannot be a blueprint's, is refused with why.
#[test]
fn a_manifest_that_does_not_read_as_a_graph_is_refused() {
    let base = || PathBuf::from("/nowhere");
    let bad_stage = "[agent]\nname = \"a\"\n\n[stages.\" padded\"]\nmode = \"autonomous\"\n";
    assert!(LoadedBlueprint::from_manifest(bad_stage, base()).is_err());
    let bad_table = "[agent]\nname = \"a\"\n\n[stages.main]\nmode = \"autonomous\"\n\n[[mcp_servers]]\nname = 7\n";
    assert!(LoadedBlueprint::from_manifest(bad_table, base()).is_err());
    let bad_name = format!(
        "[agent]\nname = \"{}\"\n\n[stages.main]\nmode = \"autonomous\"\n",
        "x".repeat(200)
    );
    let err = LoadedBlueprint::from_manifest(&bad_name, base()).unwrap_err();
    assert!(err.contains("blueprint name"), "{err}");
}
