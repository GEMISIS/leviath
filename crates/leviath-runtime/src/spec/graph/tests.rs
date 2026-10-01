use super::*;
use crate::spec::inputs::{InputSlot, InputType, InputValue, RegionBinding, Template};
use crate::spec::issues::SpecPath;
use crate::spec::names::{EdgeName, InputName, RegionName};
use leviath_core::region::{Admission, Volatility};

pub(crate) fn stage(name: &str) -> StageDef {
    StageDef {
        name: StageName::new(name).unwrap(),
        description: None,
        system_prompt: Some("work".into()),
        model: ModelChoice::default(),
        tools: vec![],
        required_tools: vec![],
        connectors: vec![],
        max_iterations: None,
        mode: StageMode::Autonomous,
        layout: None,
        hide: vec![],
        reset: vec![],
        tool_permissions: BTreeMap::new(),
        requires_children: false,
        max_revisits: None,
        transition_prompt: None,
        accepts_messages: true,
        allow_complete: false,
        allow_as_worker: false,
        allow_blocking_tools: false,
        taint_tracking: None,
        batch_tool_hint: None,
        shell_hint: None,
        nudge: None,
        sandbox: None,
        tool_routing: None,
        output_routing: BTreeMap::new(),
        output: None,
        require_output: false,
        input_accepts: vec![],
        input_as_text: vec![],
        tool_accepts: BTreeMap::new(),
        hooks: StageHooks::default(),
    }
}

pub(crate) fn region(name: &str) -> RegionDef {
    RegionDef {
        name: RegionName::new(name).unwrap(),
        kind: RegionKind::Pinned,
        budget: Budget::Tokens(1000),
        compact_at: None,
        description: None,
        describe_in_prompt: false,
        required: false,
        required_message: None,
        summarizable: true,
        admission: Admission::default(),
        volatility: Volatility::default(),
        seed: None,
        accepts: vec![],
    }
}

pub(crate) fn edge(name: &str, from: &str, to: &str) -> EdgeDef {
    EdgeDef {
        name: EdgeName::new(name).unwrap(),
        from: StageName::new(from).unwrap(),
        to: StageName::new(to).unwrap(),
        when: EdgeCondition::Always,
        hint: None,
        carry: EdgeCarry::Direct,
        gate: None,
        stuck: None,
    }
}

pub(crate) fn minimal() -> RunGraph {
    RunGraph {
        title: Some("t".into()),
        description: None,
        entry: None,
        stages: vec![stage("plan"), stage("build")],
        edges: vec![edge("next", "plan", "build")],
        layout: RegionLayoutDef {
            regions: vec![region("system"), region("task")],
            total_budget_tokens: 10_000,
            eviction_order: vec![],
        },
        inputs: vec![],
        output: None,
        compaction: None,
        max_child_depth: None,
        taint_tracking: None,
        tool_permissions: BTreeMap::new(),
        sandbox: None,
        read_paths: vec![],
        safe_commands: SafeCommandsDef::default(),
        batch_tool_hint: None,
        shell_hint: None,
        nudge: None,
        repetition: None,
        file_tracking: None,
        tool_rescan: ToolRescan::AtSpawn,
        transforms: vec![],
        mime_types: MimeRows::new(),
        dependencies: vec![],
        mcp_servers: vec![],
        script_permissions: Default::default(),
    }
}

fn codes(graph: &RunGraph) -> Vec<String> {
    match graph.validate(&SpecPath::root().field("graph")) {
        Ok(()) => vec![],
        Err(issues) => issues
            .iter()
            .map(|i| format!("{} {:?}", i.path, i.code))
            .collect(),
    }
}

#[test]
fn a_minimal_graph_is_valid_and_answers_questions() {
    let g = minimal();
    assert_eq!(codes(&g), Vec::<String>::new());
    assert_eq!(g.entry_stage().map(|s| s.name.as_str()), Some("plan"));
    assert_eq!(g.edges_from("plan").count(), 1);
    assert_eq!(g.layout_for(&g.stages[0]).regions.len(), 2);
    let mut entered = g.clone();
    entered.entry = Some(StageName::new("build").unwrap());
    assert_eq!(
        entered.entry_stage().map(|s| s.name.as_str()),
        Some("build")
    );
}

#[test]
fn every_dangling_name_is_reported() {
    let mut g = minimal();
    g.entry = Some(StageName::new("nope").unwrap());
    g.edges.push(edge("next", "plan", "gone"));
    g.stages[0].hide.push(RegionName::new("ghost").unwrap());
    g.layout
        .eviction_order
        .push(RegionName::new("ghost").unwrap());
    let got = codes(&g);
    assert_eq!(
        got,
        vec![
            "graph.entry Dangling",
            "graph.stages.plan.hide Dangling",
            "graph.edges[1].name Duplicate",
            "graph.edges[1].to Dangling",
            "graph.layout.eviction_order Dangling",
        ]
    );
}

#[test]
fn duplicates_and_empty_graphs_are_refused() {
    let mut g = minimal();
    g.stages.push(stage("plan"));
    g.layout.regions.push(region("task"));
    let got = codes(&g);
    assert!(
        got.contains(&"graph.stages[2] Duplicate".to_string()),
        "{got:?}"
    );
    assert!(
        got.contains(&"graph.layout.regions[2] Duplicate".to_string()),
        "{got:?}"
    );
    let mut empty = minimal();
    empty.stages.clear();
    empty.edges.clear();
    assert_eq!(codes(&empty), vec!["graph.stages Missing"]);
}

#[test]
fn inputs_must_fit_their_slots() {
    let mut g = minimal();
    let decl = |name: &str, ty: InputType, binds: Vec<InputSlot>| InputDecl {
        name: InputName::new(name).unwrap(),
        ty,
        required: false,
        default: None,
        description: None,
        binds,
    };
    let text = InputType::Text {
        multiline: false,
        min_len: None,
        max_len: None,
    };
    g.inputs = vec![
        decl(
            "task",
            text.clone(),
            vec![InputSlot::Region(RegionBinding {
                region: RegionName::new("task").unwrap(),
                template: Some(Template::parse("Do {task} with {missing}").unwrap()),
            })],
        ),
        decl(
            "model",
            text.clone(),
            vec![InputSlot::StageModel(StageName::new("plan").unwrap())],
        ),
        decl(
            "iters",
            InputType::Int {
                min: None,
                max: None,
            },
            vec![InputSlot::StageMaxIterations(
                StageName::new("plan").unwrap(),
            )],
        ),
        decl(
            "workers",
            InputType::Int {
                min: Some(1),
                max: None,
            },
            vec![InputSlot::FanOutMaxWorkers(StageName::new("plan").unwrap())],
        ),
        decl(
            "fmt",
            InputType::Bool,
            vec![InputSlot::OutputFormat, InputSlot::OutputInstructions],
        ),
        decl("task", text, vec![]),
    ];
    g.inputs[2].default = Some(InputValue::Text("x".into()));
    let got = codes(&g);
    assert_eq!(
        got,
        vec![
            "graph.inputs.task.binds[0].template Dangling",
            "graph.inputs.model.binds[0] WrongType",
            "graph.inputs.iters.default WrongType",
            "graph.inputs.iters.binds[0] WrongType",
            "graph.inputs.workers.binds[0] Conflict",
            "graph.inputs.fmt.binds[0] WrongType",
            "graph.inputs.fmt.binds[1] WrongType",
            "graph.inputs.task Duplicate",
        ]
    );
}

#[test]
fn fan_out_gates_carry_and_layout_refs_are_checked() {
    let mut g = minimal();
    g.stages[0].mode = StageMode::FanOut(FanOutDef {
        worker: WorkerSource::Stage(StageName::new("nobody").unwrap()),
        merge_stage: Some(StageName::new("nowhere").unwrap()),
        max_workers: 0,
        on_worker_failure: WorkerFailure::Continue,
        split_prompt: String::new(),
        results_region: Some(RegionName::new("ghost").unwrap()),
        max_items: None,
        max_attempts: None,
    });
    g.edges[0].carry = EdgeCarry::Custom {
        carry: vec![RegionName::new("ghost").unwrap()],
        compact: vec![],
        clear: vec![],
        compact_prompt: None,
    };
    g.edges[0].gate = Some(GateDef {
        region: Some(RegionName::new("ghost").unwrap()),
        ..GateDef::default()
    });
    g.edges[0].when = EdgeCondition::Stuck;
    g.layout.regions[0].kind = RegionKind::CompactHistory {
        source: RegionName::new("ghost").unwrap(),
    };
    g.layout.regions[1].budget = Budget::Percent {
        percent: 40.0,
        min: Some(10),
        max: Some(5),
    };
    g.file_tracking = Some(FileTrackingDef {
        region: RegionName::new("ghost").unwrap(),
        track_reads: true,
        track_writes: true,
        max_file_tokens: None,
    });
    g.stages[1].tool_routing = Some(ToolRoutingDef {
        default_region: RegionName::new("ghost").unwrap(),
        tool_regions: [(
            crate::spec::names::ToolName::new("bash").unwrap(),
            RegionName::new("ghost").unwrap(),
        )]
        .into(),
        keep_results: false,
        max_result_tokens: None,
        tool_max_result_tokens: BTreeMap::new(),
    });
    g.stages[1].reset.push(RegionName::new("ghost").unwrap());
    g.stages[1]
        .output_routing
        .insert("summary".into(), RegionName::new("ghost").unwrap());
    let got = codes(&g);
    assert_eq!(
        got,
        vec![
            "graph.stages.plan.mode.fan_out.merge_stage Dangling",
            "graph.stages.plan.mode.fan_out.worker Dangling",
            "graph.stages.plan.mode.fan_out.results_region Dangling",
            "graph.stages.plan.mode.fan_out.max_workers OutOfRange",
            "graph.stages.build.reset Dangling",
            "graph.stages.build.output_routing Dangling",
            "graph.stages.build.tool_routing.default_region Dangling",
            "graph.stages.build.tool_routing.tool_regions Dangling",
            "graph.edges[0].carry Dangling",
            "graph.edges[0].gate Dangling",
            "graph.edges[0].stuck Missing",
            "graph.layout.regions[0].kind.source Dangling",
            "graph.layout.regions[1].budget OutOfRange",
            "graph.layout.regions[1].budget Conflict",
            "graph.file_tracking.region Dangling",
        ]
    );
}

#[test]
fn a_stage_layout_and_interaction_points_are_checked_against_that_layout() {
    let mut g = minimal();
    g.stages[1].layout = Some(RegionLayoutDef {
        regions: vec![region("scratch")],
        total_budget_tokens: 100,
        eviction_order: vec![RegionName::new("task").unwrap()],
    });
    g.stages[1].hide.push(RegionName::new("task").unwrap());
    g.stages[1].mode = StageMode::InteractivePoints(vec![InteractionPointDef {
        name: "review".into(),
        prompt: "ok?".into(),
        required: true,
        unattended: UnattendedPoint::Ask,
        style: AnswerStyle::Confirm,
        options: vec![],
        directives: BTreeMap::new(),
        abort_options: vec![],
        edit_options: vec![],
        document_region: Some(RegionName::new("task").unwrap()),
    }]);
    let got = codes(&g);
    assert_eq!(
        got,
        vec![
            "graph.stages.build.hide Dangling",
            "graph.stages.build.mode[0].document_region Dangling",
            "graph.stages.build.layout.eviction_order Dangling",
        ]
    );
}

#[test]
fn a_graph_round_trips_through_json_toml_and_postcard() {
    let mut g = minimal();
    g.stages[0]
        .model
        .params
        .extra
        .insert("top_p".into(), ParamScalar::Float(0.9));
    g.stages[0].model.params.max_output_tokens = Some(OutputCap::RegionPercent {
        percent: 0.5,
        region: RegionName::new("task").unwrap(),
    });
    g.stages[0].tools = vec![ToolSelector::Group(ToolGroup::Builtin)];
    g.stages[0].hooks.on_stage_enter =
        Some(CodeRef::Inline("fn on_stage_enter(ctx) { () }".into()));
    g.layout.regions[1].seed = Some(Seed::Tools {
        calls: vec![SeedToolCall {
            tool: crate::spec::names::ToolName::new("list_dir").unwrap(),
            args: leviath_core::JsonDoc::new(serde_json::json!({"path": "."})),
        }],
        refresh: SeedRefresh::EachStage,
    });
    g.layout.regions[0].kind = RegionKind::SlidingWindow {
        max_items: 5,
        eviction: Eviction::Bulk(3),
    };
    g.output = Some(OutputDef {
        schema: Some(leviath_core::JsonDoc::new(
            serde_json::json!({"type": "object"}),
        )),
        ..OutputDef::default()
    });
    let json = serde_json::to_string(&g).unwrap();
    assert_eq!(serde_json::from_str::<RunGraph>(&json).unwrap(), g);
    let bin = postcard::to_stdvec(&g).unwrap();
    assert_eq!(postcard::from_bytes::<RunGraph>(&bin).unwrap(), g);
    let toml_text = toml::to_string(&g).unwrap();
    assert_eq!(toml::from_str::<RunGraph>(&toml_text).unwrap(), g);
    let hooks: Vec<&str> = g.stages[0].hooks.iter().map(|(n, _)| n).collect();
    assert_eq!(hooks, vec!["on_stage_enter"]);
}

#[test]
fn unknown_keys_are_refused() {
    let err = serde_json::from_str::<RunGraph>(
        r#"{"stages": [], "layout": {"regions": [], "total_budget_tokens": 1}, "stagse": []}"#,
    )
    .unwrap_err();
    assert!(err.to_string().contains("unknown field `stagse`"), "{err}");
}

#[test]
fn every_bundled_blueprint_reads_as_a_valid_graph() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../leviath-cli/agents");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let manifest = entry.unwrap().path().join("agent.leviath");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let bp = crate::spec::manifest::parse_manifest(&text).unwrap();
        let graph =
            RunGraph::from_blueprint(&bp).unwrap_or_else(|e| panic!("{}: {e}", manifest.display()));
        graph
            .validate(&SpecPath::root())
            .unwrap_or_else(|e| panic!("{}: {e}", manifest.display()));
        let bin = postcard::to_stdvec(&graph).unwrap();
        assert_eq!(postcard::from_bytes::<RunGraph>(&bin).unwrap(), graph);
        seen += 1;
    }
    assert!(seen >= 7, "found {seen} bundled blueprints");
}

#[test]
fn caller_input_seeds_become_text_inputs() {
    let bp = crate::spec::manifest::parse_manifest(
        r#"
[agent]
name = "t"
version = "1.0.0"
description = "d"

[context.regions]
task = { kind = "pinned", max_tokens = 100, required = true, seed = "task" }
notes = { kind = "pinned", max_tokens = 100, seed = "input" }

[stages.main]
system_prompt = "p"
"#,
    )
    .unwrap();
    let g = RunGraph::from_blueprint(&bp).unwrap();
    let names: Vec<(&str, bool)> = g
        .inputs
        .iter()
        .map(|i| (i.name.as_str(), i.required))
        .collect();
    assert_eq!(names, vec![("notes", false), ("task", true)]);
    assert!(g.layout.regions.iter().all(|r| r.seed.is_none()));
    assert_eq!(g.title.as_deref(), Some("t"));
}

#[test]
fn a_blueprint_with_bad_names_reports_each_with_its_path() {
    let mut bp = crate::spec::manifest::parse_manifest(
        "[agent]\nname = \"t\"\nversion = \"1\"\ndescription = \"\"\n[stages.main]\nsystem_prompt = \"p\"\n",
    )
    .unwrap();
    bp.stages[0].available_tools = vec!["bad tool".into(), "@all".into()];
    bp.stages[0]
        .tool_permissions
        .insert("bash".into(), "sometimes".into());
    bp.stages[0]
        .model
        .parameters
        .insert("weird".into(), serde_json::json!({"a": 1}));
    let issues = RunGraph::from_blueprint(&bp).unwrap_err();
    let lines: Vec<String> = issues
        .iter()
        .map(|i| format!("{} {:?}", i.path, i.code))
        .collect();
    assert_eq!(
        lines,
        vec![
            "stages.main.tools[0] Invalid",
            "stages.main.model.params.weird Invalid",
            "stages.main.tool_permissions.bash Invalid",
        ]
    );
}

#[test]
fn a_manifests_own_mcp_servers_and_script_permissions_are_read_into_the_graph() {
    let manifest = "[agent]\nname = \"t\"\nversion = \"1\"\n\
        [stages.main]\nsystem_prompt = \"p\"\n\
        [[mcp_servers]]\nname = \"srv\"\ncommand = \"python3\"\nargs = [\"-m\", \"srv\"]\n\
        env = { TOKEN = \"x\" }\n\
        [[mcp_servers]]\nname = \"web\"\ntransport = \"http\"\nurl = \"https://mcp.example\"\n\
        [tool_script_permissions]\nshell = \"deny\"\nhttp_get = \"inherit\"\n";
    let bp = crate::spec::manifest::parse_manifest(manifest).unwrap();
    let mut g = RunGraph::from_blueprint(&bp).unwrap();
    assert!(g.mcp_servers.is_empty());
    g.read_manifest_tables(manifest).unwrap();
    assert_eq!(g.mcp_servers.len(), 2);
    assert_eq!(g.mcp_servers[0].name.as_str(), "srv");
    assert_eq!(g.mcp_servers[0].args, ["-m", "srv"]);
    assert_eq!(g.mcp_servers[0].env["TOKEN"], "x");
    assert_eq!(g.mcp_servers[1].transport, Some(McpTransport::Http));
    assert_eq!(
        g.script_permissions,
        ScriptPermissionsDef {
            shell: Some(ScriptPermission::Deny),
            http_get: Some(ScriptPermission::Inherit),
            ..ScriptPermissionsDef::default()
        }
    );
    let back: RunGraph = toml::from_str(&toml::to_string(&g).unwrap()).unwrap();
    assert_eq!(back, g, "both read back from a blueprint file as written");

    let mut plain = g.clone();
    plain.mcp_servers.clear();
    plain
        .read_manifest_tables("[agent]\nname = \"t\"\n")
        .unwrap();
    assert!(
        plain.mcp_servers.is_empty(),
        "nothing declared, nothing read"
    );

    let bad = "[[mcp_servers]]\nname = \"bad name\"\n\
        [tool_script_permissions]\nshell = \"sometimes\"\n";
    let issues = g.clone().read_manifest_tables(bad).unwrap_err();
    let paths: Vec<String> = issues.iter().map(|i| i.path.to_string()).collect();
    assert_eq!(paths, ["mcp_servers", "script_permissions"]);
    let unreadable = g.read_manifest_tables("not [ toml").unwrap_err();
    assert_eq!(
        unreadable.0[0].code,
        crate::spec::issues::IssueCode::Invalid
    );
}
