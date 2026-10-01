use super::*;
use crate::spec::graph::tests::{region, stage};
use crate::spec::graph::{ArtifactDef, FanOutDef, GateDef, WorkerFailure, WorkerSource};
use crate::spec::names::{MimePattern, ModelId, ProviderName, RegionName, StageName, ToolName};
use crate::spec::run_spec::{StagePlan, ToolSource};

/// A parsed stage as a run graph reads it, by way of a one-stage blueprint.
pub(crate) fn stage_def_of(stage: crate::spec::Stage) -> StageDef {
    let layout = crate::spec::ContextLayout::new(Vec::new(), 1000);
    let bp = crate::spec::Blueprint::new("t".into(), "d".into(), vec![stage], layout);
    RunGraph::from_blueprint(&bp)
        .expect("a test stage reads as a graph")
        .stages
        .remove(0)
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
        assert_eq!(format!("{:?}", region_kind(&kind)), name);
    }
    let window = |e| {
        region_kind(&RegionKind::SlidingWindow {
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
        region_kind(&RegionKind::Compacting {
            threshold_tokens: None
        }),
        K::Compacting {
            threshold_tokens: usize::MAX
        }
    ));
    assert!(matches!(
        region_kind(&RegionKind::Compacting {
            threshold_tokens: Some(7)
        }),
        K::Compacting {
            threshold_tokens: 7
        }
    ));
    assert!(matches!(
        region_kind(&RegionKind::CompactHistory { source: rn("conversation") }),
        K::CompactHistory { source_region } if source_region == "conversation"
    ));
    assert!(matches!(
        region_kind(&RegionKind::Keyed {
            max_entries: Some(4)
        }),
        K::HashMap {
            max_entries: Some(4)
        }
    ));
    assert!(matches!(
        region_kind(&RegionKind::Custom { code: CodeRef::File("r.rhai".into()), pinned: true }),
        K::Custom { script, pinned: true } if script == "r.rhai"
    ));
}

#[test]
fn a_region_definition_carries_every_setting_with_its_budget() {
    let mut r = region("notes");
    r.compact_at = Some(0.5);
    r.description = Some("d".into());
    r.describe_in_prompt = true;
    r.required = true;
    r.required_message = Some("fill it".into());
    r.summarizable = false;
    r.accepts = vec![MimePattern::new("text/*").unwrap()];
    let def = region_definition(&r, 640);
    assert_eq!(
        (def.max_tokens, def.budget.clone()),
        (640, BudgetSpec::Absolute(640))
    );
    assert_eq!(def.compact_at, Some(0.5));
    assert_eq!(def.description.as_deref(), Some("d"));
    assert!(def.describe_in_prompt && def.required && !def.summarizable);
    assert_eq!(def.required_message.as_deref(), Some("fill it"));
    assert_eq!(def.accepts, vec!["text/*".to_string()]);
    assert_eq!(
        budget_spec(&Budget::Percent {
            percent: 0.5,
            min: Some(1),
            max: Some(9)
        }),
        BudgetSpec::Percent {
            percent: 0.5,
            min: Some(1),
            max: Some(9)
        }
    );
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
    let system = layout.get_region("system").unwrap();
    assert_eq!(
        system.max_tokens, 32_000,
        "half of the smaller window (64k)"
    );
    let task = layout.get_region("task").unwrap();
    assert_eq!(
        task.max_tokens, 300,
        "the build stage's planned figure is smaller than 1% of 128k"
    );
    assert_eq!(
        layout.total_budget_tokens, 64_000,
        "a percentage layout spans the widest window any region is sized for"
    );

    // A region no stage on this layout sees takes the first stage's budget.
    spec.stages[0].region_budgets.insert(rn("system"), 111);
    for s in &mut spec.graph.stages {
        s.hide = vec![rn("system")];
    }
    assert_eq!(
        graph_layout(&spec).get_region("system").unwrap().max_tokens,
        111
    );
}

#[test]
fn an_absolute_layout_keeps_its_total_and_resolves_compaction_from_its_budget() {
    let mut spec = two_stage_spec();
    spec.graph.layout.regions[0].kind = RegionKind::Compacting {
        threshold_tokens: None,
    };
    spec.graph.layout.regions[0].compact_at = Some(0.5);
    let layout = graph_layout(&spec);
    assert_eq!(layout.total_budget_tokens, 10_000);
    assert!(matches!(
        layout.get_region("system").unwrap().kind,
        leviath_core::RegionKind::Compacting {
            threshold_tokens: 500
        }
    ));
    assert_eq!(
        layout.get_region("task").unwrap().max_tokens,
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
    assert_eq!(layout.get_region("system").unwrap().max_tokens, 16_000);
    assert_eq!(layout.total_budget_tokens, 64_000);
    own.regions[0].budget = Budget::Tokens(10);
    spec.graph.stages[1].layout = Some(own);
    assert_eq!(stage_layout(&spec, 1).unwrap().total_budget_tokens, 10_000);
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
    let routing = tool_routing(&ToolRoutingDef {
        default_region: rn("results"),
        tool_regions: [(ToolName::new("grep").unwrap(), rn("hits"))].into(),
        keep_results: false,
        max_result_tokens: Some(9),
        tool_max_result_tokens: [(ToolName::new("grep").unwrap(), 3)].into(),
    });
    assert_eq!(routing.default_region, "results");
    assert_eq!(routing.tool_overrides["grep"], "hits");
    assert!(!routing.keep_results);
    assert_eq!(routing.max_result_tokens, Some(9));
    assert_eq!(routing.tool_max_result_tokens["grep"], 3);

    let nudge = nudge_config(&NudgeDef {
        enabled: Some(true),
        max: Some(2),
        text: Some("go".into()),
    });
    assert_eq!(
        (nudge.enabled, nudge.max, nudge.text.as_deref()),
        (Some(true), Some(2), Some("go"))
    );

    let ft = file_tracking(&FileTrackingDef {
        region: rn("files"),
        track_reads: false,
        track_writes: true,
        max_file_tokens: Some(5),
    });
    assert_eq!(
        (
            ft.region.as_str(),
            ft.track_reads,
            ft.track_writes,
            ft.max_file_tokens
        ),
        ("files", false, true, Some(5))
    );

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

    let routed = ModelRef::parse("p/m").unwrap();
    assert_eq!(model_entry(&routed).provider, "p");
    assert_eq!(model_entry(&ModelRef::parse("m").unwrap()).provider, "");

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
    for (cap, want) in [
        (OutputCap::Tokens(5), "Tokens(5)"),
        (OutputCap::WindowPercent(0.5), "WindowPercent(0.5)"),
        (
            OutputCap::RegionPercent {
                percent: 0.5,
                region: rn("task"),
            },
            "RegionPercent { percent: 0.5, region: \"task\" }",
        ),
    ] {
        spec.graph.stages[0].model.params.max_output_tokens = Some(cap);
        let cfg = stage_setup(&spec, 0).inference_config;
        assert_eq!(format!("{:?}", cfg.max_output_tokens.unwrap()), want);
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
    assert_eq!(setup.routing.unwrap().default_region, "task");
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
    assert_eq!(si.fallbacks[0].provider, "other");
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

#[test]
fn a_compacting_region_is_summarized_at_its_share_of_the_budget_it_is_given() {
    let mut r = region("log");
    r.kind = RegionKind::Compacting {
        threshold_tokens: None,
    };
    r.compact_at = Some(0.5);
    assert!(matches!(
        region_definition(&r, 1000).kind,
        leviath_core::RegionKind::Compacting {
            threshold_tokens: 500
        }
    ));
    r.kind = RegionKind::Compacting {
        threshold_tokens: Some(300),
    };
    assert!(matches!(
        region_definition(&r, 1000).kind,
        leviath_core::RegionKind::Compacting {
            threshold_tokens: 300
        }
    ));
    r.kind = RegionKind::Pinned;
    assert!(matches!(
        region_definition(&r, 1000).kind,
        leviath_core::RegionKind::Pinned
    ));
}

#[test]
fn a_position_past_the_last_stage_has_no_plan() {
    let spec = crate::spec::run_spec::tests::spec();
    assert_eq!(stage_inference(&spec, 9, None).model, "");
}

#[test]
fn a_stages_nudge_cascades_from_the_stage_to_the_graph_to_the_operator() {
    let mut graph = crate::spec::graph::tests::minimal();
    let operator = crate::spec::NudgeConfig {
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
