use super::*;
use crate::spec::inputs::{InputSlot, InputType, InputValue, RegionBinding, Template};
use crate::spec::issues::SpecPath;
use crate::spec::names::{EdgeName, InputName, RegionName};
use leviath_core::region::{Admission, Volatility};

pub(crate) fn stage(name: &str) -> StageDef {
    StageDef {
        routing: None,
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
        prompt_regions: Vec::new(),
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
        write_paths: Vec::new(),
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
        on_continue: None,
        evict_to: None,
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
        routing: None,
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
        write_paths: Vec::new(),
        continue_at: None,
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
fn prompt_regions_must_name_declared_regions_and_are_omitted_when_empty() {
    let mut g = minimal();
    assert!(codes(&g).is_empty());
    let plain = serde_json::to_value(&g.stages[0]).unwrap();
    assert!(plain.get("prompt_regions").is_none());

    g.stages[0]
        .prompt_regions
        .push(RegionName::new("task").unwrap());
    assert!(codes(&g).is_empty());
    let json = serde_json::to_value(&g.stages[0]).unwrap();
    assert_eq!(json["prompt_regions"], serde_json::json!(["task"]));
    let back: StageDef = serde_json::from_value(json).unwrap();
    assert_eq!(back.prompt_regions, g.stages[0].prompt_regions);

    g.stages[0]
        .prompt_regions
        .push(RegionName::new("ghost").unwrap());
    assert_eq!(codes(&g), vec!["graph.stages.plan.prompt_regions Dangling"]);
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
        merge_stage: Some(StageName::new("nowhere").unwrap()),
        max_workers: Some(0),
        on_worker_failure: WorkerFailure::Continue,
        results_region: Some(RegionName::new("ghost").unwrap()),
        max_items: Some(0),
        ..crate::test_graph::fan_out_to("plan", "nobody")
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
        source: Some(RegionName::new("ghost").unwrap()),
    };
    g.layout.regions[1].budget = Budget::Percent {
        percent: 40.0,
        min: Some(10),
        max: Some(5),
    };
    // A low-water mark is a fraction of the budget: this one is fine, the
    // next is not.
    g.layout.regions[0].evict_to = Some(0.5);
    g.layout.regions[1].evict_to = Some(1.5);
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
            "graph.stages.plan.mode.fan_out.workers_available[0].stage Dangling",
            "graph.stages.plan.mode.fan_out.results_region Dangling",
            "graph.stages.plan.mode.fan_out.max_workers OutOfRange",
            "graph.stages.plan.mode.fan_out.max_items OutOfRange",
            "graph.stages.build.reset Dangling",
            "graph.stages.build.output_routing Dangling",
            "graph.stages.build.tool_routing.default_region Dangling",
            "graph.stages.build.tool_routing.tool_regions Dangling",
            "graph.edges[0].carry Dangling",
            "graph.edges[0].gate Dangling",
            "graph.edges[0].stuck Missing",
            "graph.layout.regions[0].kind.source Dangling",
            "graph.layout.regions[1].evict_to OutOfRange",
            "graph.layout.regions[1].budget OutOfRange",
            "graph.layout.regions[1].budget Conflict",
            "graph.file_tracking.region Dangling",
        ]
    );
}

/// An idle limit of nothing would end every conversation before anybody
/// could answer; any other limit, or none, is fine.
#[test]
fn an_interactive_stage_needs_an_idle_limit_of_at_least_a_second() {
    let mut g = minimal();
    let idle = |secs| {
        StageMode::Interactive(crate::spec::graph::InteractiveDef {
            idle_timeout_secs: secs,
        })
    };
    g.stages[1].mode = idle(Some(0));
    assert_eq!(
        codes(&g),
        vec!["graph.stages.build.mode.interactive.idle_timeout_secs OutOfRange"]
    );
    for secs in [None, Some(1)] {
        g.stages[1].mode = idle(secs);
        assert!(codes(&g).is_empty());
    }
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
    let point = InteractionPointDef {
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
    };
    // A point that names no document region has nothing to check.
    let plain = InteractionPointDef {
        document_region: None,
        ..point.clone()
    };
    g.stages[1].mode = StageMode::InteractivePoints(vec![point, plain]);
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

/// A provider setting can be a table, written as a plain TOML table, kept
/// whole through every format, and sent to the provider as a JSON object.
#[test]
fn a_model_setting_can_be_a_table() {
    let params: ModelParams = toml::from_str(
        r#"extra = { thinking = { type = "enabled", budget_tokens = 2000, on = true, ratio = 0.5, stop = ["x"] } }"#,
    )
    .unwrap();
    let thinking = &params.extra["thinking"];
    assert!(matches!(thinking, ParamScalar::Table(t) if t.len() == 5));
    assert_eq!(
        thinking.to_json(),
        serde_json::json!({
            "type": "enabled", "budget_tokens": 2000, "on": true, "ratio": 0.5, "stop": ["x"],
        })
    );
    let binary = postcard::to_stdvec(&params).unwrap();
    assert_eq!(
        postcard::from_bytes::<ModelParams>(&binary).unwrap(),
        params
    );
    let json = serde_json::to_value(&params).unwrap();
    assert_eq!(json["extra"]["thinking"]["budget_tokens"], 2000);
    assert_eq!(serde_json::from_value::<ModelParams>(json).unwrap(), params);
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

/// `routing` reads from a blueprint, refuses a value it has no meaning for,
/// and is kept in both the readable and the binary form, so a run resumed
/// from its file keeps it.
#[test]
fn routing_is_read_checked_and_kept_in_every_form() {
    let graph = |extra: &str| {
        format!(
            "{extra}\nlayout = {{ regions = [], total_budget_tokens = 1 }}\n\
             [[stages]]\nname = \"a\"\nrouting = {{ on_error = \"error_edge\" }}\n"
        )
    };
    let g: RunGraph = toml::from_str(&graph("routing = { retry_attempts = 4 }")).unwrap();
    let expected = RoutingDef {
        retry_attempts: std::num::NonZeroU32::new(4),
        on_error: None,
        on_unmatched: None,
    };
    assert_eq!(g.routing, Some(expected.clone()));
    assert_eq!(
        g.stages[0].routing.as_ref().and_then(|r| r.on_error),
        Some(OnRoutingError::ErrorEdge)
    );
    assert_eq!(
        RoutingDef::attempts(g.routing.as_ref(), g.stages[0].routing.as_ref(), 3),
        4
    );
    assert_eq!(RoutingDef::attempts(None, None, 3), 3);
    assert_eq!(RoutingDef::on_error(None, None), OnRoutingError::Fail);
    // The readable forms keep it.
    let json = serde_json::to_string(&g).unwrap();
    assert_eq!(serde_json::from_str::<RunGraph>(&json).unwrap(), g);
    // So does the binary form a run file holds.
    let bin = postcard::to_stdvec(&g).unwrap();
    assert_eq!(postcard::from_bytes::<RunGraph>(&bin).unwrap(), g);

    for bad in [
        "routing = { retry_attempts = 0 }",
        "routing = { on_error = \"retry\" }",
        "routing = { tries = 2 }",
        "routing = { on_unmatched = \"guess\" }",
    ] {
        assert!(toml::from_str::<RunGraph>(&graph(bad)).is_err(), "{bad}");
    }
}

/// `on_unmatched` reads from a blueprint at either level, the stage's value
/// winning, and an unknown value is refused with the values it takes.
#[test]
fn on_unmatched_is_read_resolved_and_checked() {
    let graph = |graph: &str, stage: &str| {
        format!(
            "{graph}\nlayout = {{ regions = [], total_budget_tokens = 1 }}\n\
             [[stages]]\nname = \"a\"\n{stage}\n"
        )
    };
    let resolved = |g: &str, s: &str| {
        let g: RunGraph = toml::from_str(&graph(g, s)).unwrap();
        RoutingDef::on_unmatched(g.routing.as_ref(), g.stages[0].routing.as_ref())
    };
    assert_eq!(resolved("", ""), OnUnmatched::FirstEdge);
    assert_eq!(
        resolved("routing = { on_unmatched = \"fail\" }", ""),
        OnUnmatched::Fail
    );
    assert_eq!(
        resolved("", "routing = { on_unmatched = \"fail\" }"),
        OnUnmatched::Fail
    );
    assert_eq!(
        resolved(
            "routing = { on_unmatched = \"fail\" }",
            "routing = { on_unmatched = \"first_edge\" }"
        ),
        OnUnmatched::FirstEdge
    );
    let err = toml::from_str::<RunGraph>(&graph("routing = { on_unmatched = \"guess\" }", ""))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("unknown variant `guess`, expected `first_edge` or `fail`"),
        "{err}"
    );
}

/// The run file keeps `on_unmatched` without a new binary layout. A routing
/// block that does not set it is written byte for byte as it was before the
/// key existed, so every run file already on disk reads the same; one that
/// sets it keeps every combination through the binary form and the readable
/// one.
#[test]
fn on_unmatched_rides_in_the_binary_form_routing_always_had() {
    use std::num::NonZeroU32;
    /// `RoutingDef` as the run file wrote it before `on_unmatched`.
    #[derive(Serialize)]
    struct Before {
        retry_attempts: Option<NonZeroU32>,
        on_error: Option<OnRoutingError>,
    }
    let errors = [
        None,
        Some(OnRoutingError::Fail),
        Some(OnRoutingError::ErrorEdge),
    ];
    for retry_attempts in [None, NonZeroU32::new(3)] {
        for on_error in errors {
            let before = postcard::to_stdvec(&Before {
                retry_attempts,
                on_error,
            })
            .unwrap();
            let def = RoutingDef {
                retry_attempts,
                on_error,
                on_unmatched: None,
            };
            assert_eq!(postcard::to_stdvec(&def).unwrap(), before);
            assert_eq!(postcard::from_bytes::<RoutingDef>(&before).unwrap(), def);
            for on_unmatched in [OnUnmatched::FirstEdge, OnUnmatched::Fail] {
                let def = RoutingDef {
                    on_unmatched: Some(on_unmatched),
                    ..def.clone()
                };
                let bin = postcard::to_stdvec(&def).unwrap();
                assert_eq!(postcard::from_bytes::<RoutingDef>(&bin).unwrap(), def);
                let json = serde_json::to_string(&def).unwrap();
                assert_eq!(serde_json::from_str::<RoutingDef>(&json).unwrap(), def);
            }
        }
    }
    // The settings variant's bytes are held still too: `Some(3)`, then
    // `Some` of the variant after `ErrorEdge`, holding `Some(ErrorEdge)` and
    // `Fail`.
    let set = RoutingDef {
        retry_attempts: NonZeroU32::new(3),
        on_error: Some(OnRoutingError::ErrorEdge),
        on_unmatched: Some(OnUnmatched::Fail),
    };
    assert_eq!(postcard::to_stdvec(&set).unwrap(), [1, 3, 1, 2, 1, 1, 1]);
}

/// The `[graph]` table of each bundled blueprint's `agent.toml`, by
/// directory name. The runtime does not read the blueprint file format, so
/// this takes the one table it needs straight out of the TOML.
fn bundled_graphs() -> Vec<(String, RunGraph)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../leviath-cli/agents");
    let mut out: Vec<(String, RunGraph)> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path().join("agent.toml");
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let mut file: toml::Table = toml::from_str(&text).unwrap();
            let graph = file
                .remove("graph")
                .unwrap_or_else(|| panic!("{}: no [graph]", path.display()));
            let graph: RunGraph = graph
                .try_into()
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            (name, graph)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn every_bundled_blueprint_reads_as_a_valid_graph() {
    let all = bundled_graphs();
    assert!(all.len() >= 11, "found {} bundled blueprints", all.len());
    for (name, graph) in all {
        graph
            .validate(&SpecPath::root())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let warned = graph.warnings(&SpecPath::root()).len();
        assert_eq!(warned, 0, "{name} may never finish");
        let bin = postcard::to_stdvec(&graph).unwrap();
        assert_eq!(
            postcard::from_bytes::<RunGraph>(&bin).unwrap(),
            graph,
            "{name}"
        );
        let text = toml::to_string(&graph).unwrap();
        assert_eq!(toml::from_str::<RunGraph>(&text).unwrap(), graph, "{name}");
    }
}

/// A worker named by a path reads a blueprint's directory; anything else is
/// an installed blueprint, and a path that is not absolute is refused.
#[test]
fn a_worker_named_by_a_path_is_a_blueprint_file() {
    let dir = std::env::temp_dir().join("agents").join("fixer");
    let text = dir.to_string_lossy();
    assert_eq!(
        WorkerSource::named(&text).unwrap(),
        WorkerSource::BlueprintFile(crate::spec::names::BlueprintPath::new(&*text).unwrap())
    );
    assert_eq!(
        WorkerSource::named("fixer").unwrap(),
        WorkerSource::Blueprint(crate::spec::names::BlueprintRef::parse("fixer").unwrap())
    );
    for relative in ["./fixer", "agents/fixer", "~/fixer", "..\\fixer"] {
        assert!(stage::looks_like_a_path(relative), "{relative}");
        assert!(WorkerSource::named(relative).is_err(), "{relative}");
    }
    let json = serde_json::to_string(&WorkerSource::named(&text).unwrap()).unwrap();
    assert!(json.starts_with("{\"blueprint_file\":"), "{json}");
}

fn percent(percent: f64, min: Option<u32>, max: Option<u32>) -> Budget {
    Budget::Percent { percent, min, max }
}

#[test]
fn an_absolute_budget_ignores_the_window() {
    assert_eq!(Budget::Tokens(4000).resolve(1_000_000), 4000);
    assert_eq!(Budget::Tokens(4000).resolve(0), 4000);
}

#[test]
fn a_percentage_budget_is_its_share_of_the_window_rounded() {
    assert_eq!(percent(0.35, None, None).resolve(1_000_000), 350_000);
    assert_eq!(percent(0.5, None, None).resolve(1001), 501);
}

#[test]
fn a_percentage_budget_is_capped_then_floored_and_the_floor_wins() {
    // The cap keeps 35% of a huge window from ballooning.
    assert_eq!(percent(0.35, None, Some(4000)).resolve(1_000_000), 4000);
    // The floor keeps a small window from starving the region.
    assert_eq!(percent(0.25, Some(2000), None).resolve(4000), 2000);
    // Inside both bounds, neither applies.
    assert_eq!(
        percent(0.1, Some(10_000), Some(30_000)).resolve(200_000),
        20_000
    );
    // A floor above the cap wins.
    assert_eq!(percent(0.1, Some(9000), Some(5000)).resolve(200_000), 9000);
}

#[test]
fn an_output_cap_resolves_against_the_model_and_the_regions() {
    let budgets = |region: &str| (region == "claims").then_some(2000);
    assert_eq!(
        OutputCap::Tokens(8000).resolve(100_000, 4000, budgets),
        8000
    );
    assert_eq!(
        OutputCap::WindowPercent(0.01).resolve(100_000, 4000, budgets),
        1000
    );
    assert_eq!(
        OutputCap::WindowPercent(0.5).resolve(100_000, 4000, budgets),
        4000,
        "a relative cap is held to the model's own maximum"
    );
    let of = |region: &str| OutputCap::RegionPercent {
        percent: 0.5,
        region: RegionName::new(region).unwrap(),
    };
    assert_eq!(of("claims").resolve(100_000, 4000, budgets), 1000);
    assert_eq!(
        of("gone").resolve(100_000, 4000, budgets),
        4000,
        "a region the stage lacks asks for the model's maximum"
    );
    assert_eq!(
        OutputCap::WindowPercent(0.0).resolve(100_000, 4000, budgets),
        1,
        "never less than one token"
    );
}

#[test]
fn a_tool_group_is_written_and_read_as_its_token() {
    for group in ToolGroup::ALL {
        assert!(ToolGroup::is_token(group.token()));
        assert_eq!(ToolGroup::parse(group.token()), Some(group));
    }
    assert_eq!(ToolGroup::Mcp.token(), "@mcp");
    assert_eq!(ToolGroup::parse("read_file"), None);
    assert_eq!(ToolGroup::parse("@builtins"), None);
    assert!(ToolGroup::is_token("@builtins"));
    assert!(!ToolGroup::is_token("read_file"));
    let entries: Vec<String> = ["@mcp", "read_file", "@all", "@mcp", "@nope"]
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        ToolGroup::named_in(&entries),
        vec![ToolGroup::Mcp, ToolGroup::All]
    );
    assert!(ToolGroup::All.covers(ToolGroup::Scripts));
    assert!(ToolGroup::Scripts.covers(ToolGroup::Scripts));
    assert!(!ToolGroup::Builtin.covers(ToolGroup::Mcp));
}

#[test]
fn a_nudge_takes_each_setting_from_the_narrowest_level_that_sets_it() {
    let nothing = NudgeDef::resolve(None, None, None, false);
    assert!(nothing.enabled);
    assert_eq!(nothing.max, DEFAULT_MAX_NUDGES);
    assert_eq!(nothing.text, DEFAULT_NUDGE_TEXT);
    let global = NudgeDef {
        enabled: Some(false),
        max: Some(7),
        text: Some("global".into()),
    };
    let graph = NudgeDef {
        enabled: None,
        max: Some(5),
        text: Some("graph".into()),
    };
    let stage = NudgeDef {
        enabled: None,
        max: Some(1),
        text: None,
    };
    let got = NudgeDef::resolve(Some(&global), Some(&graph), Some(&stage), false);
    assert_eq!(
        (got.enabled, got.max, got.text.as_str()),
        (false, 1, "graph")
    );
    let got = NudgeDef::resolve(Some(&global), None, None, false);
    assert_eq!(
        (got.enabled, got.max, got.text.as_str()),
        (false, 7, "global")
    );
    // Being reviewed only changes the default for `enabled`.
    assert!(!NudgeDef::resolve(None, Some(&graph), None, true).enabled);
    let on = NudgeDef {
        enabled: Some(true),
        ..Default::default()
    };
    assert!(NudgeDef::resolve(None, None, Some(&on), true).enabled);
}

#[test]
fn a_stage_takes_its_own_input_types_or_its_regions() {
    let mut graph = minimal();
    let mime = |m: &str| crate::spec::names::MimePattern::new(m).unwrap();
    // Every region accepts anything, which reads as `*/*`, once.
    assert_eq!(
        graph.stage_inputs(&graph.stages[0]),
        vec!["*/*".to_string()]
    );
    graph.layout.regions[0].accepts = vec![mime("image/*"), mime("text/plain")];
    graph.layout.regions[1].accepts = vec![mime("application/pdf"), mime("image/*")];
    assert_eq!(
        graph.stage_inputs(&graph.stages[0]),
        vec!["image/*".to_string(), "application/pdf".to_string()],
        "text is always taken and never listed"
    );
    graph.stages[0].hide = vec![RegionName::new("system").unwrap()];
    assert_eq!(
        graph.stage_inputs(&graph.stages[0]),
        vec!["application/pdf".to_string(), "image/*".to_string()],
        "a hidden region is not read"
    );
    graph.stages[0].input_accepts = vec![mime("audio/*")];
    assert_eq!(
        graph.stage_inputs(&graph.stages[0]),
        vec!["audio/*".to_string()],
        "the stage's own list wins"
    );
}

/// An artifact whose mime type does not read is reported at its place and
/// left out of the shape.
#[test]
fn an_output_artifact_with_a_bad_mime_type_is_reported() {
    let spec = leviath_core::output::OutputSpec {
        artifacts: vec![leviath_core::output::ArtifactSpec {
            name: "chart".into(),
            mime_type: "not a type".into(),
            required: true,
            description: None,
        }],
        ..Default::default()
    };
    let issues = OutputDef::from_output_spec(&spec).unwrap_err();
    assert!(issues.to_string().contains("artifacts[0]"), "{issues}");
    let mut good = spec.clone();
    good.artifacts[0].mime_type = "image/png".into();
    let def = OutputDef::from_output_spec(&good).unwrap();
    assert_eq!(def.artifacts[0].mime_type.as_str(), "image/png");
    assert!(def.artifacts[0].required);
}

/// A fan-out's workers are checked: none at all, two of one name, one that
/// does not say what it is for; and so is the stage the fan-out names.
#[test]
fn a_fan_outs_workers_and_its_stage_are_checked() {
    let mut g = minimal();
    g.stages.push(stage("split"));
    g.stages.push(stage("work"));
    g.stages[3].allow_as_worker = true;
    let at = "graph.stages.split.mode.fan_out";
    let with = |g: &mut RunGraph, change: &dyn Fn(&mut FanOutDef)| {
        let mut fan = crate::test_graph::fan_out_to("split", "work");
        change(&mut fan);
        g.stages[2].mode = StageMode::FanOut(fan);
        codes(g)
    };
    assert_eq!(with(&mut g, &|_| {}), Vec::<String>::new());
    assert_eq!(
        with(&mut g, &|f| f.workers_available.clear()),
        [format!("{at}.workers_available Missing")]
    );
    assert_eq!(
        with(&mut g, &|f| {
            let again = f.workers_available[0].clone();
            f.workers_available.push(again);
        }),
        [format!("{at}.workers_available[1].name Duplicate")]
    );
    assert_eq!(
        with(&mut g, &|f| f.workers_available[0].use_for = " ".into()),
        [format!("{at}.workers_available[0].use_for Missing")]
    );
    assert_eq!(
        with(&mut g, &|f| f.stage = StageName::new("work").unwrap()),
        [format!("{at}.stage Conflict")]
    );
}

/// A worker's name is made from what it runs, kept to what a name allows, and
/// a worker as a blueprint writes it reads back as one, with exactly one of
/// what it runs.
#[test]
fn a_worker_is_named_and_written_as_a_blueprint_writes_it() {
    use super::{WorkerEntry, WorkerOption, worker_name_from};
    use crate::spec::names::WorkerName;
    assert_eq!(worker_name_from("my agent").as_str(), "my_agent");
    assert_eq!(worker_name_from("").as_str(), "worker");
    assert_eq!(worker_name_from("9lives").as_str(), "_9lives");
    assert_eq!(
        WorkerOption::named_after(WorkerSource::Query("a fixer".into()), "x")
            .name
            .as_str(),
        "worker"
    );

    let entry = |stage: Option<&str>, blueprint: Option<&str>, query: Option<&str>| WorkerEntry {
        name: WorkerName::new("w").unwrap(),
        stage: stage.map(|s| StageName::new(s).unwrap()),
        blueprint: blueprint.map(str::to_string),
        query: query.map(str::to_string),
        use_for: "x".into(),
    };
    let none = WorkerOption::try_from(entry(None, None, None)).unwrap_err();
    assert!(none.contains("exactly one of"), "{none}");
    let two = WorkerOption::try_from(entry(Some("s"), Some("b"), None)).unwrap_err();
    assert!(two.contains("exactly one of"), "{two}");
    let bad = WorkerOption::try_from(entry(None, Some("coder@zz"), None)).unwrap_err();
    assert!(bad.contains("blueprint 'coder@zz'"), "{bad}");

    for runs in [
        WorkerSource::Query("a fixer".into()),
        WorkerSource::BlueprintFile(
            crate::spec::names::BlueprintPath::new(
                std::env::temp_dir().join("fixer").to_string_lossy(),
            )
            .unwrap(),
        ),
    ] {
        let option = WorkerOption {
            name: WorkerName::new("w").unwrap(),
            runs: runs.clone(),
            use_for: "x".into(),
        };
        let back = WorkerOption::try_from(WorkerEntry::from(option)).unwrap();
        assert_eq!(back.runs, runs);
    }
}

/// The two refusals a graph shares with 0.6.4's validation: a gate that asks
/// for a file change on a stage that cannot change a file, and a fan-out into a
/// stage that never agreed to be a worker. Both are reported with 0.6.4's
/// wording, at the field that is wrong.
/// A write path that leaves the workdir is refused wherever it is written,
/// and the issue says what a good one looks like.
#[test]
fn write_paths_must_stay_inside_the_workdir() {
    let mut g = minimal();
    g.write_paths = vec!["docs/**".into(), "/etc/**".into()];
    g.stages[0].write_paths = vec!["../up/**".into(), "docs/panels/**".into()];
    assert_eq!(
        codes(&g),
        vec![
            "graph.write_paths[1] Invalid".to_string(),
            "graph.stages.plan.write_paths[0] Invalid".to_string(),
        ]
    );
    let issues = g.validate(&SpecPath::root().field("graph")).unwrap_err();
    let first = issues.iter().next().unwrap().to_string();
    assert!(first.contains("absolute path"), "{first}");
    assert!(first.contains("docs/**"), "{first}");
    g.write_paths.pop();
    g.stages[0].write_paths.remove(0);
    assert_eq!(codes(&g), Vec::<String>::new());
}

#[test]
fn a_gate_no_tool_can_satisfy_and_an_unwilling_worker_are_refused() {
    let mut g = minimal();
    g.edges[0].gate = Some(GateDef {
        require_modifications: true,
        ..GateDef::default()
    });
    g.stages[0].tools = vec![ToolSelector::Tool(ToolName::new("list_dir").unwrap())];
    g.stages.push(stage("split"));
    g.stages.push(stage("work"));
    g.stages[2].mode = StageMode::FanOut(crate::test_graph::fan_out_to("split", "work"));
    let issues = g.validate(&SpecPath::root().field("graph")).unwrap_err();
    let got: Vec<String> = issues.iter().map(ToString::to_string).collect();
    assert_eq!(
        got,
        vec![
            "graph.stages.split.mode.fan_out.workers_available[0].stage: conflict: worker \
             stage 'work' must set allow_as_worker = true. set allow_as_worker = true on stage \
             'work', or have this worker run a blueprint instead"
                .to_string(),
            "graph.edges[0].gate.require_modifications: conflict: gate requires modifications, \
             but the stage has no file-modifying tool in available_tools. grant 'plan' \
             write_file or edit_file, name the tool that changes files in the gate's `tools`, or \
             drop require_modifications"
                .to_string(),
        ]
    );
    // Either fix clears its refusal.
    g.stages[3].allow_as_worker = true;
    g.stages[0]
        .tools
        .push(ToolSelector::Tool(ToolName::new("edit_file").unwrap()));
    assert_eq!(codes(&g), Vec::<String>::new());
    // A tool the gate names counts when the stage has it, and so does every
    // built-in.
    g.stages[0].tools = vec![ToolSelector::Tool(ToolName::new("patch").unwrap())];
    g.edges[0].gate.as_mut().unwrap().tools = vec![ToolName::new("patch").unwrap()];
    assert_eq!(codes(&g), Vec::<String>::new());
    g.stages[0].tools = vec![ToolSelector::Group(ToolGroup::Builtin)];
    g.edges[0].gate.as_mut().unwrap().tools.clear();
    assert_eq!(codes(&g), Vec::<String>::new());
}

/// `continue_at` names a stage, and each `on_continue` can do what it says:
/// a reseed needs a seed, a summary needs a region that may be summarized
/// and a compaction model, and a region declared twice sets one policy.
#[test]
fn continue_settings_are_checked() {
    let mut g = minimal();
    g.continue_at = Some(StageName::new("nope").unwrap());
    g.layout.regions[0].on_continue = Some(OnContinue::Reseed);
    g.layout.regions[1].on_continue = Some(OnContinue::Compact);
    g.layout.regions[1].summarizable = false;
    let mut own = g.layout.clone();
    own.regions[0].on_continue = Some(OnContinue::Clear);
    own.regions[1].on_continue = Some(OnContinue::Compact);
    g.stages[1].layout = Some(own);
    assert_eq!(
        codes(&g),
        vec![
            "graph.continue_at Dangling",
            "graph.layout.regions[0].on_continue Missing",
            "graph.layout.regions[1].on_continue Conflict",
            "graph.stages.build.layout.regions[0].on_continue Conflict",
            "graph.stages.build.layout.regions[1].on_continue Conflict",
        ]
    );
    // Summarizing needs a compaction model the graph names.
    let mut g = minimal();
    g.layout.regions[1].on_continue = Some(OnContinue::Compact);
    let issues = g.validate(&SpecPath::root()).unwrap_err();
    assert!(
        issues.to_string().contains("names no compaction"),
        "{issues}"
    );
    g.compaction = Some(toml::from_str("").unwrap());
    g.layout.regions[0].seed = Some(Seed::Literal("x".into()));
    g.layout.regions[0].on_continue = Some(OnContinue::Reseed);
    g.continue_at = Some(StageName::new("build").unwrap());
    assert_eq!(codes(&g), Vec::<String>::new());
    assert_eq!(g.continue_stage().unwrap().name.as_str(), "build");
}

/// A graph and a region that set the settings a continued run reads keep
/// them through the run file's binary frames, whatever rides beside them.
#[test]
fn continue_settings_survive_the_binary_frames() {
    let mut g = minimal();
    for rescan in [
        ToolRescan::AtSpawn,
        ToolRescan::AfterWrites,
        ToolRescan::BeforeDispatch,
    ] {
        for at in [None, Some(StageName::new("build").unwrap())] {
            g.tool_rescan = rescan;
            g.continue_at = at;
            let bytes = postcard::to_stdvec(&g).unwrap();
            assert_eq!(postcard::from_bytes::<RunGraph>(&bytes).unwrap(), g);
        }
    }
    let mut r = region("r");
    for admission in [Admission::Evict, Admission::Reject] {
        for policy in [None, Some(OnContinue::Keep), Some(OnContinue::Reseed)] {
            r.admission = admission;
            r.on_continue = policy;
            let bytes = postcard::to_stdvec(&r).unwrap();
            assert_eq!(postcard::from_bytes::<RegionDef>(&bytes).unwrap(), r);
        }
    }
    // A region that sets no policy follows its kind.
    r.on_continue = None;
    r.kind = RegionKind::Clearable;
    assert_eq!(r.continue_policy(), OnContinue::Clear);
    r.kind = RegionKind::Temporary;
    assert_eq!(r.continue_policy(), OnContinue::Clear);
    r.kind = RegionKind::Pinned;
    assert_eq!(r.continue_policy(), OnContinue::Keep);
}
