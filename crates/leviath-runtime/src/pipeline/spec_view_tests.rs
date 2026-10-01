use super::*;
use crate::spec::graph::tests::{region, stage};
use crate::spec::graph::{
    ArtifactDef, FanOutDef, GateDef, OutputCap, ToolRoutingDef, WorkerFailure, WorkerSource,
};
use crate::spec::names::{
    MimePattern, ModelId, ModelRef, ProviderName, RegionName, StageName, ToolName,
};
use crate::spec::run_spec::{StagePlan, ToolSource};

/// A region of `regions` by name.
fn find<'a>(regions: &'a [leviath_core::Region], name: &str) -> &'a leviath_core::Region {
    regions
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("no region {name}"))
}

fn rn(name: &str) -> RegionName {
    RegionName::new(name).unwrap()
}

fn plan(name: &str, window: u32) -> StagePlan {
    StagePlan {
        stage: StageName::new(name).unwrap(),
        provider: ProviderName::new("p").unwrap(),
        model: ModelId::new("m").unwrap(),
        context_window: window,
        max_output_tokens: None,
        fallbacks: vec![],
        tools: vec![],
        output: None,
        region_budgets: BTreeMap::new(),
        notes: vec![],
    }
}

/// The two-stage test spec (`plan`, `build`), each stage with a plan.
fn two_stage_spec() -> RunSpec {
    let mut spec = crate::spec::run_spec::tests::spec();
    spec.stages.push(plan("build", 64_000));
    spec
}

#[test]
fn code_is_filed_under_its_path_or_its_source() {
    assert_eq!(code_key(&CodeRef::File("a.rhai".into())), "a.rhai");
    assert_eq!(code_key(&CodeRef::Inline("fn x() {}".into())), "fn x() {}");
}

#[test]
fn stages_are_found_by_name_and_the_run_starts_at_its_entry() {
    let mut graph = crate::spec::graph::tests::minimal();
    assert_eq!(stage_index(&graph, "build"), Some(1));
    assert_eq!(stage_index(&graph, "nope"), None);
    assert_eq!(entry_index(&graph), 0);
    graph.entry = Some(StageName::new("build").unwrap());
    assert_eq!(entry_index(&graph), 1);
    graph.entry = Some(StageName::new("gone").unwrap());
    assert_eq!(
        entry_index(&graph),
        0,
        "an entry naming no stage starts at the first"
    );
    let s = &graph.stages[0];
    assert_eq!(edges_from(&graph, s).len(), 1);
}

#[test]
fn a_stage_grants_tools_by_name_or_by_group() {
    let mut s = stage("s");
    assert!(!grants_all_builtins(&s));
    s.tools = vec![
        ToolSelector::Tool(ToolName::new("read_file").unwrap()),
        ToolSelector::Group(ToolGroup::Mcp),
    ];
    assert!(!grants_all_builtins(&s));
    assert!(grants_group(&s, ToolGroup::Mcp));
    assert_eq!(named_tools(&s).collect::<Vec<_>>(), vec!["read_file"]);
    s.tools.push(ToolSelector::Group(ToolGroup::All));
    assert!(grants_all_builtins(&s), "all covers the built-ins");
    s.tools = vec![ToolSelector::Group(ToolGroup::Builtin)];
    assert!(grants_all_builtins(&s));
}

#[test]
fn a_stage_sees_its_layout_and_the_runtime_regions_less_what_it_hides() {
    let mut graph = crate::spec::graph::tests::minimal();
    graph.stages[0].hide = vec![rn("task")];
    let seen = visible_regions(&graph, &graph.stages[0]);
    assert!(seen.contains("system") && seen.contains("conversation"));
    assert!(!seen.contains("task"));
}

#[test]
fn a_region_an_input_is_bound_to_is_filled_by_the_caller() {
    use crate::spec::inputs::{InputDecl, InputSlot, InputType, RegionBinding};
    let mut graph = crate::spec::graph::tests::minimal();
    assert!(!input_fills(&graph, "task"));
    graph.inputs.push(InputDecl {
        name: crate::spec::names::InputName::new("task").unwrap(),
        ty: InputType::Bool,
        required: false,
        default: None,
        description: None,
        binds: vec![
            InputSlot::OutputFormat,
            InputSlot::Region(RegionBinding {
                region: rn("task"),
                template: None,
            }),
        ],
    });
    assert!(input_fills(&graph, "task"));
    assert!(!input_fills(&graph, "system"));
}

/// A 1000-token region of `kind`, in the window's vocabulary.
fn kind_of(kind: RegionKind) -> leviath_core::RegionKind {
    let mut r = region("r");
    r.kind = kind;
    region_kind(&r, 1000)
}

#[test]
fn every_region_kind_reads_as_the_kind_the_window_keeps() {
    use leviath_core::EvictionStrategy as E;
    use leviath_core::RegionKind as K;
    let cases = [
        (RegionKind::Pinned, "Pinned"),
        (RegionKind::Temporary, "Temporary"),
        (RegionKind::Clearable, "Clearable"),
        (RegionKind::Checklist, "Checklist"),
    ];
    for (kind, name) in cases {
        assert_eq!(format!("{:?}", kind_of(kind)), name);
    }
    let window = |e| {
        kind_of(RegionKind::SlidingWindow {
            max_items: 9,
            eviction: e,
        })
    };
    assert!(matches!(
        window(Eviction::PerItem),
        K::SlidingWindow {
            max_items: 9,
            eviction_strategy: E::PerItem
        }
    ));
    assert!(matches!(
        window(Eviction::Bulk(2)),
        K::SlidingWindow {
            eviction_strategy: E::Bulk { overflow: 2 },
            ..
        }
    ));
    assert!(matches!(
        window(Eviction::Compact(3)),
        K::SlidingWindow {
            eviction_strategy: E::Compact { compact_count: 3 },
            ..
        }
    ));
    assert!(matches!(
        kind_of(RegionKind::Compacting {
            threshold_tokens: None
        }),
        K::Compacting {
            threshold_tokens: 800
        }
    ));
    assert!(matches!(
        kind_of(RegionKind::Compacting {
            threshold_tokens: Some(7)
        }),
        K::Compacting {
            threshold_tokens: 7
        }
    ));
    assert!(matches!(
        kind_of(RegionKind::CompactHistory { source: Some(rn("conversation")) }),
        K::CompactHistory { source_region } if source_region == "conversation"
    ));
    assert!(matches!(
        kind_of(RegionKind::CompactHistory { source: None }),
        K::CompactHistory { source_region } if source_region.is_empty()
    ));
    assert!(matches!(
        kind_of(RegionKind::Keyed {
            max_entries: Some(4)
        }),
        K::HashMap {
            max_entries: Some(4)
        }
    ));
    assert!(matches!(
        kind_of(RegionKind::Custom { code: CodeRef::File("r.rhai".into()), pinned: true }),
        K::Custom { script, pinned: true } if script == "r.rhai"
    ));
}

#[test]
fn a_window_region_carries_every_setting_with_its_budget() {
    let mut r = region("notes");
    r.compact_at = Some(0.5);
    r.description = Some("d".into());
    r.describe_in_prompt = true;
    r.summarizable = false;
    r.accepts = vec![MimePattern::new("text/*").unwrap()];
    let region = crate::context_setup::region_from_def(&r, 640);
    assert_eq!(region.max_tokens, 640);
    assert_eq!(region.description.as_deref(), Some("d"));
    assert!(region.describe_in_prompt && !region.summarizable);
    assert_eq!(region.accepts, vec!["text/*".to_string()]);
}

#[test]
fn the_graph_layout_sizes_each_region_for_the_narrowest_stage_that_sees_it() {
    let mut spec = two_stage_spec();
    spec.stages[0].region_budgets.clear();
    spec.graph.layout.regions[0].budget = Budget::Percent {
        percent: 0.5,
        min: None,
        max: None,
    };
    // `task` is planned at 300 by the build stage; plan's budget for it is
    // worked out from its own window.
    spec.stages[1].region_budgets.insert(rn("task"), 300);
    spec.graph.layout.regions[1].budget = Budget::Percent {
        percent: 0.01,
        min: None,
        max: None,
    };
    let layout = graph_layout(&spec);
    let system = find(&layout.regions, "system");
    assert_eq!(
        system.max_tokens, 32_000,
        "half of the smaller window (64k)"
    );
    let task = find(&layout.regions, "task");
    assert_eq!(
        task.max_tokens, 300,
        "the build stage's planned figure is smaller than 1% of 128k"
    );
    assert_eq!(
        layout.total, 64_000,
        "a percentage layout spans the widest window any region is sized for"
    );

    // A region no stage on this layout sees takes the first stage's budget.
    spec.stages[0].region_budgets.insert(rn("system"), 111);
    for s in &mut spec.graph.stages {
        s.hide = vec![rn("system")];
    }
    assert_eq!(find(&graph_layout(&spec).regions, "system").max_tokens, 111);
}

#[test]
fn an_absolute_layout_keeps_its_total_and_resolves_compaction_from_its_budget() {
    let mut spec = two_stage_spec();
    spec.graph.layout.regions[0].kind = RegionKind::Compacting {
        threshold_tokens: None,
    };
    spec.graph.layout.regions[0].compact_at = Some(0.5);
    let layout = graph_layout(&spec);
    assert_eq!(layout.total, 10_000);
    assert!(matches!(
        find(&layout.regions, "system").kind,
        leviath_core::RegionKind::Compacting {
            threshold_tokens: 500
        }
    ));
    assert_eq!(
        find(&layout.regions, "task").max_tokens,
        500,
        "the plan's figure"
    );
}

#[test]
fn a_stage_layout_is_sized_for_that_stage() {
    let mut spec = two_stage_spec();
    assert!(stage_layout(&spec, 0).is_none(), "no layout of its own");
    assert!(stage_layout(&spec, 9).is_none(), "no such stage");
    let mut own = spec.graph.layout.clone();
    own.regions[0].budget = Budget::Percent {
        percent: 0.25,
        min: None,
        max: None,
    };
    spec.graph.stages[1].layout = Some(own.clone());
    let layout = stage_layout(&spec, 1).unwrap();
    assert_eq!(find(&layout, "system").max_tokens, 16_000);
    own.regions[0].budget = Budget::Tokens(10);
    spec.graph.stages[1].layout = Some(own);
    assert_eq!(
        find(&stage_layout(&spec, 1).unwrap(), "system").max_tokens,
        10
    );
}

#[test]
fn a_stage_with_no_plan_is_budgeted_against_the_fallback_window() {
    let spec = crate::spec::run_spec::tests::spec();
    let r = RegionDef {
        budget: Budget::Percent {
            percent: 0.5,
            min: None,
            max: None,
        },
        ..region("x")
    };
    assert_eq!(budget_in(&spec, 1, &r), FALLBACK_WINDOW as usize / 2);
    assert_eq!(budget_in(&spec, 0, &r), 64_000);
}

#[test]
fn settings_read_in_the_vocabulary_their_systems_use() {
    let out = output_spec(&OutputDef {
        format: Some("json".into()),
        schema: Some(leviath_core::JsonDoc::new(
            serde_json::json!({"type": "object"}),
        )),
        validator: Some(CodeRef::File("v.rhai".into())),
        artifacts: vec![ArtifactDef {
            name: "a.png".into(),
            mime_type: MimePattern::new("image/png").unwrap(),
            required: true,
            description: Some("pic".into()),
        }],
        ..Default::default()
    });
    assert_eq!(out.schema, Some(serde_json::json!({"type": "object"})));
    assert_eq!(out.validator.as_deref(), Some("v.rhai"));
    assert_eq!(out.artifacts[0].mime_type, "image/png");

    assert_eq!(ModelRef::parse("p/m").unwrap().provider_or_empty(), "p");
    assert_eq!(ModelRef::parse("m").unwrap().provider_or_empty(), "");

    let t = tool(&ToolDef {
        name: ToolName::new("t").unwrap(),
        description: "does".into(),
        schema: leviath_core::JsonDoc::new(serde_json::json!({"type": "object"})),
        source: ToolSource::Builtin,
    });
    assert_eq!((t.name.as_str(), t.description.as_str()), ("t", "does"));
}

#[test]
fn model_settings_reach_the_request_as_written() {
    let mut spec = two_stage_spec();
    let params = &mut spec.graph.stages[0].model.params;
    params.temperature = Some(0.2);
    params.extra = [
        ("a".to_string(), ParamScalar::Bool(true)),
        ("b".to_string(), ParamScalar::Int(3)),
        ("c".to_string(), ParamScalar::Float(0.5)),
        ("d".to_string(), ParamScalar::Text("x".into())),
        ("e".to_string(), ParamScalar::TextList(vec!["y".into()])),
    ]
    .into();
    spec.graph.stages[0].input_as_text = vec![MimePattern::new("text/csv").unwrap()];
    spec.graph.stages[0].model.request_timeout_secs = Some(30);
    for cap in [
        OutputCap::Tokens(5),
        OutputCap::WindowPercent(0.5),
        OutputCap::RegionPercent {
            percent: 0.5,
            region: rn("task"),
        },
    ] {
        spec.graph.stages[0].model.params.max_output_tokens = Some(cap.clone());
        let cfg = stage_setup(&spec, 0).inference_config;
        assert_eq!(cfg.max_output_tokens, Some(cap));
    }
    let cfg = stage_setup(&spec, 0).inference_config;
    assert_eq!(cfg.temperature, Some(0.2));
    assert_eq!(
        serde_json::Value::Object(cfg.extra_params),
        serde_json::json!({"a": true, "b": 3, "c": 0.5, "d": "x", "e": ["y"]})
    );
    assert_eq!(cfg.as_text, vec!["text/csv".to_string()]);
    assert_eq!(cfg.request_timeout_secs, Some(30));
    assert!(
        cfg.batch_tool_hint && cfg.shell_hint,
        "unset hints default on"
    );
    spec.graph.batch_tool_hint = Some(false);
    spec.graph.stages[0].shell_hint = Some(false);
    let cfg = stage_setup(&spec, 0).inference_config;
    assert!(!cfg.batch_tool_hint && !cfg.shell_hint);
}

fn fan_out(split: &str) -> StageMode {
    StageMode::FanOut(FanOutDef {
        worker: WorkerSource::Stage(StageName::new("build").unwrap()),
        merge_stage: None,
        max_workers: 2,
        on_worker_failure: WorkerFailure::Continue,
        split_prompt: split.to_string(),
        results_region: None,
        max_items: None,
        max_attempts: None,
    })
}

#[test]
fn a_stage_prompt_folds_in_its_split_and_its_output_demand() {
    let mut spec = two_stage_spec();
    let prompt = |spec: &RunSpec| stage_setup(spec, 0).system_prompt;
    assert_eq!(prompt(&spec).as_deref(), Some("work"));
    spec.graph.stages[0].mode = fan_out("split it");
    assert_eq!(prompt(&spec).as_deref(), Some("work\n\nsplit it"));
    spec.graph.stages[0].system_prompt = None;
    assert_eq!(prompt(&spec).as_deref(), Some("split it"));
    spec.graph.stages[0].mode = fan_out("  ");
    assert_eq!(prompt(&spec), None, "a blank split is no prompt");

    // Owing an output, with a shape to describe and without one.
    spec.graph.stages[0].require_output = true;
    spec.stages[0].output = Some(OutputDef::default());
    let demand = prompt(&spec).unwrap();
    assert!(demand.starts_with("Before this stage ends you must call `submit_output`"));
    assert!(demand.ends_with("the only thing the caller receives."));
    spec.stages[0].output = Some(OutputDef {
        format: Some("json".into()),
        ..Default::default()
    });
    spec.graph.stages[0].system_prompt = Some("base".into());
    let demand = prompt(&spec).unwrap();
    assert!(demand.starts_with("base\n\nBefore this stage ends"));
    assert!(demand.contains("json"));
    spec.stages[0].output = None;
    assert_eq!(
        prompt(&spec).as_deref(),
        Some("base"),
        "nothing to demand without a shape"
    );
}

#[test]
fn a_stage_is_entered_with_what_its_definition_says() {
    let mut spec = two_stage_spec();
    let s = &mut spec.graph.stages[1];
    s.accepts_messages = false;
    s.hide = vec![rn("system")];
    s.reset = vec![rn("task")];
    s.tool_routing = Some(ToolRoutingDef {
        default_region: rn("task"),
        tool_regions: BTreeMap::new(),
        keep_results: true,
        max_result_tokens: None,
        tool_max_result_tokens: BTreeMap::new(),
    });
    let setup = stage_setup(&spec, 1);
    assert!(!setup.accepts_messages);
    assert_eq!(setup.context_hide, vec!["system".to_string()]);
    assert_eq!(setup.context_reset, vec!["task".to_string()]);
    assert_eq!(setup.routing.unwrap().default_region.as_str(), "task");
    assert!(setup.context_layout.is_none());
    let missing = stage_setup(&spec, 7);
    assert!(missing.system_prompt.is_none() && !missing.accepts_messages);
}

#[test]
fn a_stage_calls_what_its_plan_says_unless_its_tools_were_looked_up_again() {
    let spec = crate::spec::run_spec::tests::spec();
    let si = stage_inference(&spec, 0, None);
    assert_eq!(
        (si.provider_name.as_str(), si.model.as_str()),
        ("mock", "gpt-mock")
    );
    assert_eq!(si.tools.len(), 3);
    assert_eq!(si.fallbacks[0].provider_or_empty(), "other");
    assert!(si.output.is_some());
    let overrides = StageToolOverrides([(0, vec![tool(&spec.stages[0].tools[0])])].into());
    assert_eq!(stage_inference(&spec, 0, Some(&overrides)).tools.len(), 1);
    assert_eq!(
        stage_inference(&spec, 1, Some(&overrides)).provider_name,
        "",
        "no plan"
    );
}

#[test]
fn a_graph_can_modify_files_through_a_tool_a_group_or_a_gate() {
    let mut graph = crate::spec::graph::tests::minimal();
    assert!(!any_stage_can_modify(&graph));
    graph.stages[0].tools = vec![ToolSelector::Tool(ToolName::new("read_file").unwrap())];
    assert!(!any_stage_can_modify(&graph));
    graph.edges[0].gate = Some(GateDef {
        tools: vec![ToolName::new("read_file").unwrap()],
        ..Default::default()
    });
    assert!(
        any_stage_can_modify(&graph),
        "a gate counts its tools as writes"
    );
    graph.edges[0].gate = None;
    graph.stages[1].tools = vec![ToolSelector::Tool(ToolName::new("edit_file").unwrap())];
    assert!(any_stage_can_modify(&graph));
    graph.stages[1].tools = vec![ToolSelector::Group(ToolGroup::Builtin)];
    assert!(any_stage_can_modify(&graph));
}

/// A one-stage graph whose stage grants `tools` (a group by its token), with
/// `gate_tools` named by the gate on an edge leaving it. `None` gives the
/// stage no edge at all; an empty list gives it an edge with no gate.
fn writing_graph(tools: &[&str], gate_tools: Option<&[&str]>) -> crate::spec::graph::RunGraph {
    let mut graph = crate::spec::graph::tests::minimal();
    graph.stages.truncate(1);
    graph.edges.clear();
    graph.stages[0].tools = tools
        .iter()
        .map(|t| match ToolGroup::parse(t) {
            Some(g) => ToolSelector::Group(g),
            None => ToolSelector::Tool(ToolName::new(*t).unwrap()),
        })
        .collect();
    if let Some(extra) = gate_tools {
        let mut edge = crate::spec::graph::tests::edge("next", "plan", "plan");
        edge.gate = (!extra.is_empty()).then(|| GateDef {
            require_modifications: true,
            tools: extra.iter().map(|t| ToolName::new(*t).unwrap()).collect(),
            ..Default::default()
        });
        graph.edges.push(edge);
    }
    graph
}

#[test]
fn whether_any_stage_could_have_written_is_asked_of_its_tools() {
    let none = |graph: crate::spec::graph::RunGraph| !any_stage_can_modify(&graph);
    // A graph with no stages offers nothing.
    let mut empty = writing_graph(&[], None);
    empty.stages.clear();
    assert!(none(empty));
    // Read-only, and the sub-agent tools a router would use: nothing the
    // framework tracks as a file change.
    assert!(none(writing_graph(
        &["read_file", "spawn_agent", "context_write"],
        None
    )));
    // `shell` confers no tracked write: an agent editing through `sed -i`
    // leaves no record, so silence from it stays suspicious rather than
    // excused. The alias resolves, so `bash` is judged as `shell`.
    assert!(none(writing_graph(&["bash"], None)));
    // A built-in group carries `write_file` and `edit_file` unnamed.
    assert!(!none(writing_graph(&["@builtin"], None)));
    assert!(none(writing_graph(&["@scripts"], None)));
    // A built-in modifying tool, under either name.
    assert!(!none(writing_graph(&["write_file"], None)));
    assert!(!none(writing_graph(&["edit_file"], None)));
    // Only one stage needs it.
    let mut two = writing_graph(&["read_file"], None);
    let mut second = two.stages[0].clone();
    second.name = crate::spec::names::StageName::new("write").unwrap();
    second.tools = vec![ToolSelector::Tool(ToolName::new("write_file").unwrap())];
    two.stages.push(second);
    assert!(!none(two));
}

#[test]
fn a_gate_declaring_its_own_write_tool_counts_as_a_write() {
    let none = |graph: crate::spec::graph::RunGraph| !any_stage_can_modify(&graph);
    // An MCP/script write tool the stage advertises AND a gate names is a
    // tracked write - the same escape hatch `stage_modifying_tools` gives.
    assert!(!none(writing_graph(
        &["mcp__fs__put"],
        Some(&["mcp__fs__put"])
    )));
    // Declared by the gate but never advertised: the stage cannot call it.
    assert!(none(writing_graph(&["read_file"], Some(&["mcp__fs__put"]))));
    // An edge, but no gate on it.
    assert!(none(writing_graph(&["read_file"], Some(&[]))));
    // A gate that names a tool unrelated to what the stage advertises.
    assert!(none(writing_graph(
        &["mcp__fs__put"],
        Some(&["mcp__other__put"])
    )));
}

#[test]
fn a_compacting_region_is_summarized_at_its_share_of_the_budget_it_is_given() {
    let mut r = region("log");
    r.kind = RegionKind::Compacting {
        threshold_tokens: None,
    };
    r.compact_at = Some(0.5);
    assert!(matches!(
        region_kind(&r, 1000),
        leviath_core::RegionKind::Compacting {
            threshold_tokens: 500
        }
    ));
    r.kind = RegionKind::Compacting {
        threshold_tokens: Some(300),
    };
    assert!(matches!(
        region_kind(&r, 1000),
        leviath_core::RegionKind::Compacting {
            threshold_tokens: 300
        }
    ));
    r.kind = RegionKind::Pinned;
    assert!(matches!(
        region_kind(&r, 1000),
        leviath_core::RegionKind::Pinned
    ));
}

/// A compacting region that names neither a threshold nor a share is
/// summarized at 80% of its budget, as a blueprint region written that way
/// always was: a fixed budget's 80% rounded down, a share of the window's
/// 80% rounded to the nearest token.
#[test]
fn a_compacting_region_with_no_trigger_is_summarized_at_four_fifths_of_its_budget() {
    let mut r = region("log");
    r.kind = RegionKind::Compacting {
        threshold_tokens: None,
    };
    let threshold = |r: &RegionDef| match region_kind(r, 1001) {
        leviath_core::RegionKind::Compacting { threshold_tokens } => threshold_tokens,
        other => panic!("{other:?}"),
    };
    assert_eq!(threshold(&r), 800);
    r.budget = Budget::Percent {
        percent: 0.5,
        min: None,
        max: None,
    };
    assert_eq!(threshold(&r), 801);
}

#[test]
fn a_position_past_the_last_stage_has_no_plan() {
    let spec = crate::spec::run_spec::tests::spec();
    assert_eq!(stage_inference(&spec, 9, None).model, "");
}

#[test]
fn a_stages_nudge_cascades_from_the_stage_to_the_graph_to_the_operator() {
    let mut graph = crate::spec::graph::tests::minimal();
    let operator = NudgeDef {
        enabled: None,
        max: Some(7),
        text: Some("operator".into()),
    };
    let nudge = stage_nudge(&graph, None, Some(&operator));
    assert!(nudge.enabled);
    assert_eq!((nudge.max, nudge.text.as_str()), (7, "operator"));
    graph.nudge = Some(NudgeDef {
        text: Some("graph".into()),
        ..Default::default()
    });
    graph.stages[0].nudge = Some(NudgeDef {
        max: Some(1),
        ..Default::default()
    });
    let nudge = stage_nudge(&graph, Some(&graph.stages[0]), Some(&operator));
    assert_eq!((nudge.max, nudge.text.as_str()), (1, "graph"));

    // A reviewed stage is not nudged unless something says to.
    graph.stages[0].mode =
        StageMode::InteractivePoints(vec![crate::spec::graph::InteractionPointDef {
            name: "approve".into(),
            prompt: "ok?".into(),
            required: true,
            unattended: Default::default(),
            style: Default::default(),
            options: vec![],
            directives: BTreeMap::new(),
            abort_options: vec![],
            edit_options: vec![],
            document_region: None,
        }]);
    assert!(!stage_nudge(&graph, Some(&graph.stages[0]), None).enabled);
    graph.stages[0].nudge = Some(NudgeDef {
        enabled: Some(true),
        ..Default::default()
    });
    assert!(stage_nudge(&graph, Some(&graph.stages[0]), None).enabled);
    graph.stages[0].mode = StageMode::InteractivePoints(vec![]);
    graph.stages[0].nudge = None;
    assert!(
        stage_nudge(&graph, Some(&graph.stages[0]), None).enabled,
        "a stage with no points is not reviewed"
    );
}
