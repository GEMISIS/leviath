//! Reading blueprints as run graphs: every setting a blueprint can hold, and
//! every way one can fail to fit.

use std::collections::{BTreeMap, HashMap};

use leviath_core::lifecycle::CompactionConfig;
use leviath_core::mime::TokenRule as CoreTokenRule;
use leviath_core::mime::registry::MimeRow;
use leviath_core::output::{ArtifactSpec, OutputSpec};
use leviath_core::region::{EvictionStrategy, RegionKind as CoreKind};
use serde_json::json;

use super::*;
use crate::spec::blueprint::{
    ContentTransform as BpContent, ContextTransform, Dependency, DependencyInstall, DependencyKind,
    EdgeTransform, FanOutConfig, FileTrackingConfig, InteractionPoint, InteractionStyle,
    McpServerTemplate as BpServer, ModelConfig, ModelEntry, NudgeConfig, ReadPathsConfig,
    RegionCount as BpCount, RegionMapping, RepetitionDetectionConfig, SafeCommandsConfig, Stage,
    StageMode as BpMode, ToolRescan as BpRescan, ToolResultRouting, TransitionCondition,
    TransitionEdge, TransitionGate, UnattendedPolicy, WorkerFailurePolicy,
};
use crate::spec::layout::{BudgetSpec, ContextLayout, RegionDefinition, SeedToolCall};

fn model() -> ModelConfig {
    ModelConfig::new("p".to_string(), "m".to_string())
}

fn stage(name: &str) -> Stage {
    Stage::new(name.to_string(), model())
}

fn region_def(name: &str, kind: CoreKind) -> RegionDefinition {
    RegionDefinition::new(name.to_string(), kind, 1000)
}

fn blueprint(stages: Vec<Stage>, regions: Vec<RegionDefinition>) -> Blueprint {
    Blueprint::new(
        "t".into(),
        "d".into(),
        stages,
        ContextLayout::new(regions, 10_000),
    )
}

fn point(style: InteractionStyle, unattended: UnattendedPolicy) -> InteractionPoint {
    InteractionPoint {
        name: "p".into(),
        prompt: "ok?".into(),
        required: true,
        unattended,
        style,
        options: vec!["yes".into()],
        directives: HashMap::from([("yes".to_string(), "go".to_string())]),
        abort_options: vec![],
        edit_options: vec![],
        document_region: Some("notes".into()),
    }
}

fn fan_out(agent: Option<&str>, stage: Option<&str>, query: Option<&str>) -> BpMode {
    BpMode::FanOut {
        config: FanOutConfig {
            worker_agent: agent.map(str::to_string),
            worker_stage: stage.map(str::to_string),
            worker_query: query.map(str::to_string),
            merge_stage: Some("main".into()),
            max_workers: 2,
            on_worker_failure: WorkerFailurePolicy::FailAll,
            split_prompt: "split".into(),
            results_region: Some("notes".into()),
            max_items: Some(9),
            max_attempts: Some(2),
        },
    }
}

fn routing(default_region: &str, tool: &str) -> ToolResultRouting {
    ToolResultRouting {
        default_region: default_region.into(),
        tool_overrides: HashMap::from([(tool.to_string(), "notes".to_string())]),
        keep_results: true,
        max_result_tokens: Some(100),
        tool_max_result_tokens: HashMap::from([(tool.to_string(), 50)]),
    }
}

fn mime_row(tokens: CoreTokenRule) -> toml::Value {
    toml::Value::try_from(MimeRow {
        family: Some("doc".into()),
        tokens: Some(tokens),
        check: Some("check.rhai".into()),
        ..MimeRow::default()
    })
    .unwrap()
}

fn dependency(kind: DependencyKind) -> Dependency {
    Dependency {
        name: kind.tag().into(),
        kind,
        required: true,
        remedy: Some("install it".into()),
        description: None,
        install: Some(DependencyInstall {
            command: Some("brew install x".into()),
            commands: BTreeMap::new(),
            script: Some("install.rhai".into()),
            server: Some(BpServer {
                command: Some("x".into()),
                args: vec!["--stdio".into()],
                ..BpServer::default()
            }),
        }),
    }
}

/// A blueprint that sets every setting the conversion reads, all of them
/// well formed.
fn rich() -> Blueprint {
    let mut main = stage("main");
    main.model.models = vec![
        ModelEntry::new("p".into(), "m".into()),
        ModelEntry::new(String::new(), "bare".into()),
    ];
    main.model.parameters = HashMap::from([
        ("temperature".to_string(), json!(0.5)),
        ("max_output_tokens".to_string(), json!("50% of notes")),
        ("stop".to_string(), json!(["a", "b"])),
        ("seed".to_string(), json!(7)),
        ("top_p".to_string(), json!(0.9)),
        ("cache".to_string(), json!(true)),
        ("tier".to_string(), json!("fast")),
    ]);
    main.available_tools = vec![
        "@subagent".into(),
        "@scripts".into(),
        "@mcp".into(),
        "read_file".into(),
    ];
    main.mode = BpMode::InteractivePoints {
        points: vec![
            point(InteractionStyle::FreeText, UnattendedPolicy::Ask),
            point(InteractionStyle::Confirm, UnattendedPolicy::AutoApprove),
            point(InteractionStyle::MultipleChoice, UnattendedPolicy::Ask),
        ],
    };
    main.nudge = Some(NudgeConfig {
        enabled: Some(true),
        max: Some(2),
        text: Some("go on".into()),
    });
    main.sandbox = Some(Default::default());
    main.tool_result_routing = Some(routing("notes", "read_file"));
    main.output_routing = BTreeMap::from([("report".to_string(), "notes".to_string())]);
    main.tool_accepts = BTreeMap::from([("read_file".to_string(), vec!["text/*".to_string()])]);
    main.tool_permissions = HashMap::from([("shell".to_string(), "deny".to_string())]);
    main.transitions = Some(HashMap::from([(
        "ask".to_string(),
        TransitionEdge {
            target: "ask".into(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Clear,
            gate: Some(TransitionGate {
                require_region_entries: Some(BpCount {
                    region: "notes".into(),
                    at_least: 1,
                }),
                ..TransitionGate::default()
            }),
            stuck: None,
        },
    )]));
    let mut ask = stage("ask");
    ask.mode = BpMode::Interactive;
    let mut fan = stage("fan");
    fan.mode = fan_out(None, None, Some("find the work"));

    let mut notes = region_def(
        "notes",
        CoreKind::SlidingWindow {
            max_items: 4,
            eviction_strategy: EvictionStrategy::Compact { compact_count: 2 },
        },
    );
    notes.seed = Some(RegionSeed::Glob {
        pattern: "*.md".into(),
    });
    let mut keyed = region_def(
        "keyed",
        CoreKind::HashMap {
            max_entries: Some(3),
        },
    );
    keyed.seed = Some(RegionSeed::Rhai {
        script: "seed.rhai".into(),
    });
    let mut tools = region_def("tools", CoreKind::Checklist);
    tools.seed = Some(RegionSeed::Tools {
        calls: vec![SeedToolCall::new("read_file")],
        refresh: crate::spec::layout::SeedRefresh::EachStage,
    });
    let mut pct = region_def(
        "log",
        CoreKind::Compacting {
            threshold_tokens: usize::MAX,
        },
    );
    pct.budget = BudgetSpec::Percent {
        percent: 0.2,
        min: Some(10),
        max: Some(500),
    };
    let regions = vec![
        notes,
        keyed,
        tools,
        pct,
        region_def(
            "history",
            CoreKind::CompactHistory {
                source_region: "log".into(),
            },
        ),
        region_def(
            "archive",
            CoreKind::CompactHistory {
                source_region: String::new(),
            },
        ),
        region_def(
            "custom",
            CoreKind::Custom {
                script: "r.rhai".into(),
                pinned: true,
            },
        ),
    ];
    let mut b = blueprint(vec![main, ask, fan], regions);
    b.metadata.insert("tool_perm:shell".into(), json!("deny"));
    b.output = Some(OutputSpec {
        format: Some("json".into()),
        artifacts: vec![ArtifactSpec {
            name: "chart".into(),
            mime_type: "image/png".into(),
            required: true,
            description: None,
        }],
        ..OutputSpec::default()
    });
    b.read_paths = Some(ReadPathsConfig {
        allow: vec!["~/notes/**".into()],
    });
    b.safe_commands = Some(SafeCommandsConfig {
        tools: vec!["read_file".into()],
        shell: vec!["ls".into()],
    });
    b.file_tracking = Some(FileTrackingConfig {
        region: "notes".into(),
        track_reads: true,
        track_writes: false,
        max_file_tokens: Some(100),
    });
    b.repetition_detection = Some(RepetitionDetectionConfig {
        max_repeat_calls: Some(3),
        max_readonly_streak: None,
        enabled: Some(true),
    });
    b.sandbox = Some(Default::default());
    b.tool_rescan = BpRescan::AfterWrites;
    b.transforms = vec![ContextTransform {
        from_blueprint: "a".into(),
        to_blueprint: "b".into(),
        mappings: [
            None,
            Some(BpContent::Direct),
            Some(BpContent::Summarize),
            Some(BpContent::Extract {
                fields: vec!["x".into()],
            }),
        ]
        .into_iter()
        .map(|transform| RegionMapping {
            from_region: "notes".into(),
            to_region: "notes".into(),
            transform,
        })
        .collect(),
    }];
    b.dependencies = vec![
        dependency(DependencyKind::McpServer {
            server: "github".into(),
            env: vec!["TOKEN".into()],
        }),
        dependency(DependencyKind::Env { var: "KEY".into() }),
        dependency(DependencyKind::Binary {
            command: "git".into(),
        }),
        dependency(DependencyKind::Script {
            check: "check.rhai".into(),
        }),
    ];
    b.compaction_config = Some(CompactionConfig {
        provider: "p".into(),
        model: "small".into(),
        system_prompt: None,
        user_prompt_template: None,
        max_summary_tokens: 500,
        temperature: 0.2,
    });
    let rows = [
        CoreTokenRule::PerByte(0.25),
        CoreTokenRule::PerPixel {
            divisor: 750,
            max: 1600,
        },
        CoreTokenRule::PerSecond(32),
        CoreTokenRule::PerPage(800),
        CoreTokenRule::Fixed(100),
    ];
    for (i, rule) in rows.into_iter().enumerate() {
        b.mime_types
            .insert(format!("application/x-{i}"), mime_row(rule));
    }
    b
}

#[test]
fn every_setting_a_blueprint_holds_converts() {
    let g = RunGraph::from_blueprint(&rich()).unwrap();
    assert_eq!(g.tool_permissions.len(), 1);
    assert_eq!(g.output.as_ref().unwrap().artifacts.len(), 1);
    assert_eq!(g.read_paths, vec!["~/notes/**".to_string()]);
    assert_eq!(g.safe_commands.shell, vec!["ls".to_string()]);
    assert!(g.file_tracking.is_some() && g.repetition.is_some() && g.sandbox.is_some());
    assert_eq!(g.tool_rescan, ToolRescan::AfterWrites);
    assert_eq!(g.transforms[0].mappings.len(), 4);
    assert_eq!(g.dependencies.len(), 4);
    assert_eq!(g.compaction.as_ref().unwrap().model.model.as_str(), "small");
    assert_eq!(g.mime_types.len(), 5);

    let main = g.stage("main").unwrap();
    assert_eq!(main.model.models.len(), 2);
    assert_eq!(main.model.models[1].provider, None);
    assert_eq!(
        main.model.params.extra.get("stop"),
        Some(&ParamScalar::TextList(vec!["a".into(), "b".into()]))
    );
    assert_eq!(
        main.model.params.max_output_tokens,
        Some(OutputCap::RegionPercent {
            percent: 0.5,
            region: RegionName::new("notes").unwrap(),
        })
    );
    assert_eq!(main.tools.len(), 4);
    assert!(main.nudge.is_some() && main.sandbox.is_some() && main.tool_routing.is_some());
    assert_eq!(main.output_routing.len(), 1);
    assert_eq!(main.tool_accepts.len(), 1);
    assert_eq!(g.stage("ask").unwrap().mode, StageMode::Interactive);
    // Main's declared edge, and ask's fall-through to fan.
    assert_eq!(g.edges.len(), 2);

    let kind = |name: &str| {
        g.layout
            .regions
            .iter()
            .find(|r| r.name.as_str() == name)
            .map(|r| r.kind.clone())
    };
    assert_eq!(
        kind("archive"),
        Some(RegionKind::CompactHistory { source: None })
    );
    assert_eq!(
        kind("log"),
        Some(RegionKind::Compacting {
            threshold_tokens: None
        })
    );
    assert_eq!(
        kind("keyed"),
        Some(RegionKind::Keyed {
            max_entries: Some(3)
        })
    );
    assert_eq!(kind("tools"), Some(RegionKind::Checklist));
    g.validate(&SpecPath::root()).unwrap();
}

#[test]
fn every_tool_rescan_converts() {
    let mut b = blueprint(vec![stage("main")], vec![]);
    b.tool_rescan = BpRescan::BeforeDispatch;
    let g = RunGraph::from_blueprint(&b).unwrap();
    assert_eq!(g.tool_rescan, ToolRescan::BeforeDispatch);
}

/// A stage with no `transitions` table goes on along the fall-through edge,
/// and is never offered the choice to end the run, since without declared
/// edges it never chooses.
#[test]
fn a_stage_with_no_table_gets_the_fall_through_edge_and_no_choice() {
    let mut a = stage("a");
    a.allow_complete = true;
    let mut b = stage("b");
    b.transitions = Some(HashMap::new());
    b.allow_complete = true;
    let mut c = stage("c");
    c.allow_complete = true;
    let g = RunGraph::from_blueprint(&blueprint(vec![a, b, c], vec![])).unwrap();
    assert_eq!(g.edges.len(), 1);
    let edge = &g.edges[0];
    assert_eq!(
        (edge.name.as_str(), edge.from.as_str(), edge.to.as_str()),
        (FALL_THROUGH_EDGE, "a", "b")
    );
    assert_eq!(
        (edge.when, &edge.carry, &edge.gate),
        (EdgeCondition::Always, &EdgeCarry::Direct, &None)
    );
    let complete: Vec<bool> = g.stages.iter().map(|s| s.allow_complete).collect();
    assert_eq!(complete, vec![false, true, false]);
}

/// The paths of every issue, sorted.
fn paths(b: &Blueprint) -> Vec<String> {
    let issues = RunGraph::from_blueprint(b).unwrap_err();
    let mut out: Vec<String> = issues.0.iter().map(|i| i.path.to_string()).collect();
    out.sort();
    out.dedup();
    out
}

/// Every graph-wide setting that names something badly is reported at its
/// own path, all at once.
#[test]
fn every_badly_named_graph_setting_is_reported_where_it_sits() {
    let mut b = rich();
    b.metadata
        .insert("tool_perm:bad tool!".into(), json!("deny"));
    b.output.as_mut().unwrap().artifacts[0].mime_type = "nope".into();
    b.file_tracking.as_mut().unwrap().region = " bad".into();
    b.transforms.push(ContextTransform {
        from_blueprint: " bad".into(),
        to_blueprint: "b".into(),
        mappings: vec![],
    });
    b.transforms.push(ContextTransform {
        from_blueprint: "a".into(),
        to_blueprint: " bad".into(),
        mappings: vec![],
    });
    b.transforms.push(ContextTransform {
        from_blueprint: "a".into(),
        to_blueprint: "b".into(),
        mappings: vec![
            RegionMapping {
                from_region: " bad".into(),
                to_region: "notes".into(),
                transform: None,
            },
            RegionMapping {
                from_region: "notes".into(),
                to_region: " bad".into(),
                transform: None,
            },
        ],
    });
    b.dependencies[0].kind = DependencyKind::McpServer {
        server: "bad name".into(),
        env: vec![],
    };
    b.compaction_config.as_mut().unwrap().model = "has space".into();
    b.mime_types
        .insert("nope".into(), mime_row(CoreTokenRule::Fixed(1)));
    b.mime_types
        .insert("text/x-bad".into(), toml::Value::String("row".into()));
    b.max_child_depth = Some(1000);
    assert_eq!(
        paths(&b),
        vec![
            "compaction.model",
            "dependencies[0].server",
            "file_tracking.region",
            "mime_types.nope",
            "mime_types[\"text/x-bad\"]",
            "output.artifacts[0]",
            "tool_permissions[\"bad tool!\"]",
            "transforms[1].from",
            "transforms[2].to",
            "transforms[3].mappings[0].from",
            "transforms[3].mappings[1].to",
        ]
    );
}

/// Every stage setting that names something badly, or holds a number too
/// big or a shape no stage takes, is reported at its own path.
#[test]
fn every_bad_stage_setting_is_reported_where_it_sits() {
    let mut main = stage("main");
    main.model.models = vec![
        ModelEntry::new("has space".into(), "m".into()),
        ModelEntry::new("p".into(), "has space".into()),
    ];
    main.model.parameters = HashMap::from([
        ("max_output_tokens".to_string(), json!("lots")),
        ("mixed".to_string(), json!([1, "a"])),
    ]);
    main.max_iterations = Some(u32::MAX as usize + 1);
    main.output_routing = BTreeMap::from([("report".to_string(), " bad".to_string())]);
    main.tool_accepts = BTreeMap::from([("bad tool!".to_string(), vec!["text/*".to_string()])]);
    main.tool_result_routing = Some(routing(" bad", "bad tool!"));
    let edge = |target: &str, gate_region: &str| TransitionEdge {
        target: target.into(),
        condition: TransitionCondition::Always,
        hint: None,
        transform: EdgeTransform::Direct,
        gate: Some(TransitionGate {
            require_region_entries: Some(BpCount {
                region: gate_region.into(),
                at_least: 1,
            }),
            ..TransitionGate::default()
        }),
        stuck: None,
    };
    main.transitions = Some(HashMap::from([
        (" bad".to_string(), edge("main", "tools")),
        ("aimless".to_string(), edge(" bad", "tools")),
        ("gated".to_string(), edge("main", " bad")),
    ]));
    let mut capped = stage("capped");
    capped.model.parameters =
        HashMap::from([("max_output_tokens".to_string(), json!("50% of a\tb"))]);
    let mut agent = stage("agent");
    agent.mode = fan_out(Some(" bad"), None, None);
    let mut worker = stage("worker");
    worker.mode = fan_out(None, Some(" bad"), None);
    let mut both = stage("both");
    both.mode = fan_out(Some("a"), Some("b"), None);
    let mut nameless = stage(" bad");
    nameless.transitions = Some(HashMap::from([(
        "main".to_string(),
        TransitionEdge {
            target: "main".into(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Direct,
            gate: None,
            stuck: None,
        },
    )]));

    let mut tools = region_def("tools", CoreKind::Checklist);
    tools.seed = Some(RegionSeed::Tools {
        calls: vec![SeedToolCall::new("bad tool!")],
        refresh: Default::default(),
    });
    let mut input = region_def("input", CoreKind::Pinned);
    input.seed = Some(RegionSeed::CallerInput { name: "9x".into() });
    let regions = vec![
        region_def(" bad", CoreKind::Pinned),
        region_def(
            "history",
            CoreKind::CompactHistory {
                source_region: " bad".into(),
            },
        ),
        tools,
        input,
    ];
    let b = blueprint(vec![main, capped, agent, worker, both, nameless], regions);
    assert_eq!(
        paths(&b),
        vec![
            "layout.regions[0].name",
            "layout.regions[1].kind",
            "layout.regions[2].seed[0]",
            "layout.regions[3].seed",
            "stages.agent.mode.fan_out.worker",
            "stages.both.mode.fan_out.worker",
            "stages.capped.model.params.max_output_tokens",
            "stages.main.max_iterations",
            "stages.main.model.models[0].provider",
            "stages.main.model.models[1].model",
            "stages.main.model.params.max_output_tokens",
            "stages.main.model.params.mixed",
            "stages.main.output_routing.report",
            "stages.main.tool_accepts[\"bad tool!\"]",
            "stages.main.tool_routing.default_region",
            "stages.main.tool_routing.tool_max_result_tokens[\"bad tool!\"]",
            "stages.main.tool_routing.tool_regions[\"bad tool!\"]",
            "stages.main.transitions.aimless.target",
            "stages.main.transitions.gated.gate.require_region_entries.region",
            "stages.main.transitions[\" bad\"]",
            "stages.worker.mode.fan_out.worker",
            "stages[\" bad\"].name",
            "stages[\" bad\"].transitions.main",
        ]
    );
}
