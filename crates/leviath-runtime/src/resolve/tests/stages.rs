use super::*;
use crate::spec::graph::{Budget, CompactionDef, OutputCap, OutputDef, RegionKind};
use crate::spec::launch::Unattended;

fn percent(p: f64, min: Option<u32>, max: Option<u32>) -> Budget {
    Budget::Percent {
        percent: p,
        min,
        max,
    }
}

#[tokio::test]
async fn percentage_budgets_are_sized_against_the_narrowest_stage_that_sees_them() {
    let mut g = graph();
    g.layout.regions[0].budget = percent(0.1, None, None);
    g.layout.regions[1].budget = percent(0.5, Some(1_000), Some(20_000));
    g.stages.push(StageDef {
        name: n("wide"),
        hide: vec![n("system")],
        ..g.stages[0].clone()
    });
    let mut own = g.layout.clone();
    own.regions[0].budget = percent(0.01, Some(5_000), None);
    g.stages.push(StageDef {
        name: n("own"),
        layout: Some(own),
        ..g.stages[0].clone()
    });
    let env = Fake {
        models: [
            ("plan".to_string(), Ok(plan("mock", "a", 64_000))),
            ("build".to_string(), Ok(plan("mock", "b", 128_000))),
            ("wide".to_string(), Ok(plan("mock", "c", 8_000))),
            ("own".to_string(), Ok(plan("mock", "d", 200_000))),
        ]
        .into(),
        ..Fake::default()
    };
    let resolved = spawn(&raw(g), &env).await.unwrap();
    let budgets = |i: usize| resolved.spec.stages[i].region_budgets.clone();
    // `system` is hidden from `wide`, so the narrowest window seeing it is 64k.
    assert_eq!(budgets(0)["system"], 6_400);
    assert_eq!(budgets(2)["system"], 6_400);
    // `task` is seen by `wide` too: half of 8k, over its 1k floor.
    assert_eq!(budgets(0)["task"], 4_000);
    // A stage with its own layout uses its own window, and a floor wins.
    assert_eq!(budgets(3)["system"], 5_000);
    assert_eq!(budgets(3)["task"], 20_000, "capped at max");
}

#[tokio::test]
async fn a_region_nobody_sees_takes_the_first_window() {
    let mut g = graph();
    g.layout.regions[0].budget = percent(0.5, None, None);
    for stage in &mut g.stages {
        stage.hide = vec![n("system")];
    }
    let resolved = spawn(&raw(g), &Fake::default()).await.unwrap();
    assert_eq!(resolved.spec.stages[1].region_budgets["system"], 50_000);
}

#[tokio::test]
async fn a_stage_left_no_room_to_work_is_refused() {
    let mut g = graph();
    g.layout.regions[0].budget = Budget::Tokens(95_000);
    let mut own = g.layout.clone();
    own.regions[1].kind = RegionKind::Custom {
        code: CodeRef::Inline("fn render() {}".into()),
        pinned: true,
    };
    own.regions[1].budget = Budget::Tokens(10_000);
    own.regions[0].budget = Budget::Tokens(85_000);
    g.stages[1].layout = Some(own);
    let issues = spawn(&raw(g), &Fake::default()).await.unwrap_err();
    assert_eq!(
        found(&issues),
        [
            "source.raw.layout OutOfRange",
            "source.raw.stages.build.layout OutOfRange",
        ]
    );
    assert!(
        issues.0[0].message.contains("only 4000 tokens"),
        "{}",
        issues.0[0].message
    );
}

#[tokio::test]
async fn a_small_window_is_not_held_to_the_working_room_floor() {
    let mut g = graph();
    g.layout.regions[0].budget = Budget::Tokens(9_000);
    let env = Fake {
        models: [
            ("plan".to_string(), Ok(plan("mock", "tiny", 10_000))),
            ("build".to_string(), Ok(plan("mock", "tiny", 10_000))),
        ]
        .into(),
        ..Fake::default()
    };
    spawn(&raw(g), &env).await.unwrap();
}

#[tokio::test]
async fn output_caps_resolve_against_the_window_the_regions_and_the_models_most() {
    let mut g = graph();
    g.stages.push(StageDef {
        name: n("three"),
        ..g.stages[0].clone()
    });
    g.stages.push(StageDef {
        name: n("four"),
        ..g.stages[0].clone()
    });
    let caps = [
        OutputCap::Tokens(512),
        OutputCap::WindowPercent(0.25),
        OutputCap::RegionPercent {
            percent: 0.5,
            region: n("task"),
        },
        OutputCap::RegionPercent {
            percent: 0.5,
            region: n("elsewhere"),
        },
    ];
    for (stage, cap) in g.stages.iter_mut().zip(caps) {
        stage.model.params.max_output_tokens = Some(cap);
    }
    let resolved = spawn(&raw(g), &Fake::default()).await.unwrap();
    let caps: Vec<Option<u32>> = resolved
        .spec
        .stages
        .iter()
        .map(|s| s.max_output_tokens)
        .collect();
    // A quarter of the 100k window is more than the model writes in one reply
    // (4096), and a cap on a region the stage lacks is the model's own most.
    assert_eq!(caps, [Some(512), Some(4096), Some(500), Some(4096)]);
}

#[tokio::test]
async fn an_unattended_run_loses_the_tools_that_wait_on_a_person() {
    let mut g = graph();
    g.stages[0].required_tools = vec![n("ask_user_text")];
    let everything = vec![
        tool("read_file"),
        tool("ask_user_text"),
        tool("ask_user_choice"),
    ];
    let env = Fake {
        tools: [
            ("plan".to_string(), Ok(everything.clone())),
            ("build".to_string(), Ok(everything)),
        ]
        .into(),
        ..Fake::default()
    };
    let mut request = raw(g);
    request.launch.unattended = Unattended::All;
    let resolved = spawn(&request, &env).await.unwrap();
    let names = |i: usize| -> Vec<String> {
        resolved.spec.stages[i]
            .tools
            .iter()
            .map(|t| t.name.to_string())
            .collect()
    };
    assert_eq!(names(0), ["read_file", "ask_user_text"]);
    assert_eq!(names(1), ["read_file"]);
}

#[tokio::test]
async fn a_required_tool_the_machine_lacks_is_refused_with_what_it_has() {
    let mut g = graph();
    g.stages[1].required_tools = vec![n("read_file"), n("gh__search")];
    let env = Fake {
        tools: [
            ("build".to_string(), Ok(vec![tool("read_file")])),
            (
                "plan".to_string(),
                Err(SpawnIssue::new(
                    SpecPath::root().field("tools").key("gh"),
                    IssueCode::Unavailable,
                    "server gh is not connected",
                )
                .into()),
            ),
        ]
        .into(),
        ..Fake::default()
    };
    let issues = spawn(&raw(g), &env).await.unwrap_err();
    assert_eq!(
        found(&issues),
        [
            "source.raw.stages.plan.tools.gh Unavailable",
            "source.raw.stages.build.required_tools[1] Unresolvable",
        ]
    );
    assert_eq!(issues.0[1].known, ["read_file"]);
}

#[tokio::test]
async fn submit_output_is_told_the_shape_each_stage_asks_for() {
    let mut g = graph();
    g.output = Some(OutputDef {
        format: Some("markdown".into()),
        ..OutputDef::default()
    });
    g.stages[1].output = Some(OutputDef {
        instructions: Some("one line".into()),
        ..OutputDef::default()
    });
    let tools = vec![tool("read_file"), tool(leviath_tools::SUBMIT_OUTPUT_TOOL)];
    let env = Fake {
        tools: [
            ("plan".to_string(), Ok(tools.clone())),
            ("build".to_string(), Ok(tools)),
        ]
        .into(),
        ..Fake::default()
    };
    let resolved = spawn(&raw(g), &env).await.unwrap();
    let submit = |i: usize| resolved.spec.stages[i].tools[1].description.clone();
    assert!(
        submit(0).contains("Return it in this format: markdown."),
        "{}",
        submit(0)
    );
    assert!(submit(1).contains("one line"), "{}", submit(1));
    assert_eq!(
        resolved.spec.stages[0].tools[0].description,
        "read_file does things"
    );
}

#[tokio::test]
async fn an_empty_output_shape_keeps_the_tools_own_wording() {
    let mut g = graph();
    g.output = Some(OutputDef::default());
    let env = Fake {
        tools: [(
            "plan".to_string(),
            Ok(vec![tool(leviath_tools::SUBMIT_OUTPUT_TOOL)]),
        )]
        .into(),
        ..Fake::default()
    };
    let resolved = spawn(&raw(g), &env).await.unwrap();
    assert_eq!(
        resolved.spec.stages[0].tools[0].description,
        "submit_output does things"
    );
}

#[tokio::test]
async fn the_callers_model_reaches_only_stages_that_allow_it() {
    let mut g = graph();
    g.stages[1].model.allow_user_default = false;
    let mut request = raw(g);
    request.model = Some(model("other/big"));
    let env = Fake::default();
    let resolved = spawn(&request, &env).await.unwrap();
    assert_eq!(
        env.asked(),
        [
            ("plan".to_string(), Some(model("other/big"))),
            ("build".to_string(), None),
        ]
    );
    assert_eq!(resolved.spec.requested_model, Some(model("other/big")));
    assert_eq!(resolved.spec.stages[0].provider.as_str(), "other");
}

#[tokio::test]
async fn the_fingerprint_covers_every_provider_and_server_the_run_uses() {
    let mut g = graph();
    g.compaction = Some(CompactionDef {
        model: model("summer/small"),
        system_prompt: None,
        user_prompt_template: None,
        max_summary_tokens: 500,
        temperature: 0.2,
    });
    g.stages[0].connectors = vec![n("linear")];
    let mut chosen = plan("mock", "m", 100_000);
    chosen.fallbacks = vec![model("backup/m"), model("bare")];
    chosen.notes = vec!["[model] moved".into()];
    let mcp = ToolDef {
        source: ToolSource::Mcp {
            server: n("gh"),
            tool: "search".into(),
        },
        ..tool("gh__search")
    };
    let env = Fake {
        models: [("plan".to_string(), Ok(chosen))].into(),
        tools: [("build".to_string(), Ok(vec![mcp, tool("read_file")]))].into(),
        printed: ["mock", "backup", "summer", "gh", "linear", "sandbox:plan"]
            .map(String::from)
            .into(),
        ..Fake::default()
    };
    let resolved = spawn(&raw(g), &env).await.unwrap();
    let fp = &resolved.spec.env;
    let sandboxed: Vec<(&str, _)> = fp.sandbox.iter().map(|(s, k)| (s.as_str(), *k)).collect();
    assert_eq!(
        sandboxed,
        [("plan", leviath_core::sandbox::SandboxKind::Container)],
        "the sandbox of each stage the host runs one for"
    );
    let providers: Vec<&str> = fp.providers.keys().map(|p| p.as_str()).collect();
    assert_eq!(providers, ["backup", "mock", "summer"]);
    let servers: Vec<&str> = fp.mcp_servers.keys().map(|s| s.as_str()).collect();
    assert_eq!(servers, ["gh", "linear"]);
    assert_eq!(resolved.spec.stages[0].notes, ["[model] moved"]);
    assert_eq!(resolved.spec.stages[0].fallbacks.len(), 2);
}
