//! The operator's defaults, unattended profiles, and the run's own code and
//! mime rows reaching every question the resolver asks.

use std::path::PathBuf;

use super::*;
use crate::spec::graph::{CompactionDef, InstallDef, MimeRowDef, Needs, NudgeDef, TokenRule};
use crate::spec::names::ProfileName;
use crate::spec::request::{Attachment, Bytes};
use crate::state::context::PartBody;

fn ceilings(resolved: &Resolved) -> Vec<Option<u32>> {
    resolved
        .spec
        .graph
        .stages
        .iter()
        .map(|s| s.max_iterations)
        .collect()
}

#[tokio::test]
async fn the_operators_iteration_ceiling_fills_a_stage_that_sets_none_or_zero() {
    let mut g = graph();
    g.stages[0].max_iterations = Some(5);
    g.stages[1].max_iterations = Some(0);
    let mut env = Fake::default();
    env.limits.default_max_iterations = Some(7);
    let resolved = spawn(&raw(g.clone()), &env).await.unwrap();
    assert_eq!(ceilings(&resolved), [Some(5), Some(7)]);
    g.stages[1].max_iterations = None;
    let resolved = spawn(&raw(g.clone()), &env).await.unwrap();
    assert_eq!(ceilings(&resolved), [Some(5), Some(7)]);

    let resolved = spawn(&raw(g), &Fake::default()).await.unwrap();
    assert_eq!(
        ceilings(&resolved),
        [Some(5), None],
        "no ceiling, none added"
    );
}

#[tokio::test]
async fn the_operators_hints_nudge_and_taint_fill_what_the_graph_leaves_open() {
    let mut g = graph();
    g.batch_tool_hint = Some(false);
    g.nudge = Some(NudgeDef {
        enabled: Some(false),
        max: None,
        text: None,
    });
    let mut env = Fake::default();
    env.limits.defaults = OperatorDefaults {
        batch_tool_hint: true,
        shell_hint: false,
        nudge: NudgeDef {
            enabled: Some(true),
            max: Some(3),
            text: Some("go on".into()),
        },
        taint_tracking: true,
        capture_model_input: false,
    };
    let spec = spawn(&raw(g.clone()), &env).await.unwrap().spec;
    assert_eq!(
        spec.graph.batch_tool_hint,
        Some(false),
        "the graph's own stands"
    );
    assert_eq!(spec.graph.shell_hint, Some(false));
    assert_eq!(
        spec.graph.nudge,
        Some(NudgeDef {
            enabled: Some(false),
            max: Some(3),
            text: Some("go on".into()),
        })
    );
    assert_eq!(spec.graph.taint_tracking, Some(true));

    g.taint_tracking = Some(false);
    let spec = spawn(&raw(g.clone()), &env).await.unwrap().spec;
    assert_eq!(
        spec.graph.taint_tracking,
        Some(true),
        "a graph cannot turn the operator's taint tracking off"
    );
    let spec = spawn(&raw(g.clone()), &Fake::default()).await.unwrap().spec;
    assert_eq!(spec.graph.taint_tracking, Some(false));
    assert_eq!(spec.graph.shell_hint, Some(true));
    g.taint_tracking = Some(true);
    let spec = spawn(&raw(g), &Fake::default()).await.unwrap().spec;
    assert_eq!(
        spec.graph.taint_tracking,
        Some(true),
        "a graph may turn it on"
    );
}

#[tokio::test]
async fn the_operator_can_record_every_runs_model_input() {
    let mut env = Fake::default();
    env.limits.defaults.capture_model_input = true;
    let spec = spawn(&raw(graph()), &env).await.unwrap().spec;
    assert!(spec.launch.capture_model_input);
    let child = Caller::Child {
        parent: n("p-1"),
        policy: spec.launch.clone(),
        depth: 0,
    };
    let resolved = resolve(&raw(graph()), &child, &env, ResolveMode::Spawn)
        .await
        .unwrap();
    assert!(resolved.spec.launch.capture_model_input);
    let plain = spawn(&raw(graph()), &Fake::default()).await.unwrap().spec;
    assert!(!plain.launch.capture_model_input);
}

fn asking_tools() -> Fake {
    let everything = vec![tool("read_file"), tool("ask_user_text")];
    Fake {
        tools: [
            ("plan".to_string(), Ok(everything.clone())),
            ("build".to_string(), Ok(everything)),
        ]
        .into(),
        profiles: [
            (
                "quiet".to_string(),
                AutoAnswers {
                    questions: true,
                    checkpoints: false,
                    gate: true,
                },
            ),
            ("chatty".to_string(), AutoAnswers::default()),
        ]
        .into(),
        ..Fake::default()
    }
}

fn names(spec: &crate::spec::run_spec::RunSpec, stage: usize) -> Vec<String> {
    spec.stages[stage]
        .tools
        .iter()
        .map(|t| t.name.to_string())
        .collect()
}

#[tokio::test]
async fn a_named_profile_decides_what_the_run_answers_for_itself() {
    let env = asking_tools();
    let mut request = raw(graph());
    request.launch.unattended = Unattended::Profile(n::<ProfileName>("quiet"));
    let spec = spawn(&request, &env).await.unwrap().spec;
    assert_eq!(
        spec.auto_answers,
        AutoAnswers {
            questions: true,
            checkpoints: false,
            gate: true,
        }
    );
    assert_eq!(
        names(&spec, 0),
        ["read_file"],
        "nobody answers its questions"
    );

    request.launch.unattended = Unattended::Profile(n("chatty"));
    let spec = spawn(&request, &env).await.unwrap().spec;
    assert_eq!(spec.auto_answers, AutoAnswers::default());
    assert_eq!(names(&spec, 0), ["read_file", "ask_user_text"]);

    request.launch.unattended = Unattended::All;
    let spec = spawn(&request, &env).await.unwrap().spec;
    assert_eq!(spec.auto_answers, AutoAnswers::all());
}

#[tokio::test]
async fn a_profile_the_machine_lacks_is_refused_with_the_ones_it_has() {
    let mut request = raw(graph());
    request.launch.unattended = Unattended::Profile(n("nope"));
    let issues = spawn(&request, &asking_tools()).await.unwrap_err();
    assert_eq!(found(&issues), ["launch.unattended Unresolvable"]);
    assert_eq!(issues.0[0].known, ["chatty", "quiet"]);
}

#[tokio::test]
async fn a_compaction_model_the_machine_will_not_send_context_to_is_refused() {
    let mut g = graph();
    g.compaction = Some(CompactionDef {
        model: model("openai/gpt-5.5"),
        system_prompt: None,
        user_prompt_template: None,
        max_summary_tokens: 500,
        temperature: 0.0,
    });
    assert!(spawn(&raw(g.clone()), &Fake::default()).await.is_ok());
    let env = Fake {
        compaction_refusal: Some("openai/gpt-5.5, which keeps what it is sent".into()),
        ..Fake::default()
    };
    let issues = spawn(&raw(g), &env).await.unwrap_err();
    assert_eq!(found(&issues), ["source.raw.compaction.model NotAllowed"]);
    assert!(
        issues.0[0]
            .message
            .starts_with("the compaction model is openai/gpt-5.5")
    );
    assert_eq!(issues.0[0].got.as_deref(), Some("openai/gpt-5.5"));
}

fn text_file(name: &str) -> Attachment {
    Attachment {
        name: name.into(),
        mime_type: None,
        region: None,
        deliver: None,
        caption: None,
        data: Bytes(b"plain words".to_vec()),
    }
}

#[tokio::test]
async fn attachments_are_typed_and_charged_by_the_runs_own_mime_rows() {
    let mut g = graph();
    g.mime_types.insert(
        n("text/plain"),
        MimeRowDef {
            tokens: Some(TokenRule::Fixed(77)),
            stand_in: Some("[a note called {name}]".into()),
            ..MimeRowDef::default()
        },
    );
    let mut request = raw(g.clone());
    request.attachments.push(text_file("a.txt"));
    let resolved = spawn(&request, &Fake::default()).await.unwrap();
    let PartBody::Stored(blob) = &resolved.spec.seeded["task"].parts[0].body else {
        panic!("an attachment is stored")
    };
    assert_eq!(blob.tokens, 77);
    assert!(
        blob.stand_in.contains("a note called a.txt"),
        "{}",
        blob.stand_in
    );

    let env = Fake {
        mime_refusal: Some("row text/plain: unknown field".into()),
        ..Fake::default()
    };
    let issues = spawn(&request, &env).await.unwrap_err();
    assert_eq!(found(&issues), ["source.raw.mime_types Invalid"]);
}

#[tokio::test]
async fn a_dependency_judged_by_code_is_handed_the_runs_copy() {
    let mut g = graph();
    let check = "fn check() { () }";
    g.dependencies = vec![
        DependencyDef {
            name: "probe".into(),
            needs: Needs::Check(CodeRef::Inline(check.into())),
            required: true,
            remedy: None,
            description: None,
            install: None,
        },
        DependencyDef {
            name: "key".into(),
            needs: Needs::Env("KEY".into()),
            required: true,
            remedy: None,
            description: None,
            install: None,
        },
    ];
    let env = Fake::default();
    spawn(&raw(g), &env).await.unwrap();
    assert_eq!(
        *env.dep_code.lock().unwrap(),
        [Some(check.as_bytes().to_vec()), None]
    );
}

#[tokio::test]
async fn an_install_script_is_checked_for_what_an_install_calls() {
    let mut g = graph();
    g.dependencies = vec![DependencyDef {
        name: "tool".into(),
        needs: Needs::Binary("tool".into()),
        required: true,
        remedy: None,
        description: None,
        install: Some(InstallDef {
            script: Some(CodeRef::Inline("fn check() {}".into())),
            ..InstallDef::default()
        }),
    }];
    let issues = spawn(&raw(g), &Fake::default()).await.unwrap_err();
    assert_eq!(
        found(&issues),
        ["source.raw.dependencies[0].install.script Invalid"]
    );
}

#[tokio::test]
async fn script_tool_code_the_machine_found_travels_with_the_run() {
    let global = (
        CodeRef::File("/home/me/.leviath/tools/g.rhai".into()),
        b"// @tool g\n1".to_vec(),
    );
    let mut g_tool = tool("g");
    g_tool.source = ToolSource::Script(Digest::of(&global.1));
    let env = Fake {
        tools: [
            ("plan".to_string(), Ok(vec![g_tool.clone()])),
            ("build".to_string(), Ok(vec![g_tool])),
        ]
        .into(),
        tool_code: [
            ("plan".to_string(), vec![global.clone()]),
            ("build".to_string(), vec![global.clone()]),
        ]
        .into(),
        ..Fake::default()
    };
    let resolved = spawn(&raw(graph()), &env).await.unwrap();
    let digest = Digest::of(&global.1);
    assert_eq!(resolved.code[&digest], global.1);
    assert_eq!(resolved.spec.code_digest(&global.0), Some(&digest));
    assert_eq!(
        resolved
            .spec
            .code
            .iter()
            .filter(|(c, _)| *c == global.0)
            .count(),
        1,
        "recorded once however many stages use it"
    );
    assert_eq!(*env.tool_bases.lock().unwrap(), [None, None]);
}

#[tokio::test]
async fn a_blueprints_tools_are_looked_up_beside_it() {
    let env = Fake {
        blueprints: [(
            "coder".to_string(),
            LoadedBlueprint {
                graph: graph(),
                reference: BlueprintRef::parse("coder").unwrap(),
                version: "1".into(),
                base_dir: PathBuf::from("/agents/coder"),
            },
        )]
        .into(),
        ..Fake::default()
    };
    let request = SpawnRequest::new(SpawnSource::Blueprint(
        BlueprintRef::parse("coder").unwrap(),
    ))
    .input("task", RawInput::Text("go".into()));
    spawn(&request, &env).await.unwrap();
    assert_eq!(
        *env.tool_bases.lock().unwrap(),
        [
            Some(PathBuf::from("/agents/coder")),
            Some(PathBuf::from("/agents/coder"))
        ]
    );
}

#[tokio::test]
async fn a_seed_sees_the_run_it_seeds() {
    let mut g = graph();
    g.layout.regions[0].seed = Some(Seed::Literal("hello".into()));
    let mut request = raw(g.clone());
    request.launch.unattended = Unattended::All;
    let env = Fake::default();
    let resolved = spawn(&request, &env).await.unwrap();
    assert_eq!(
        *env.seed_sights.lock().unwrap(),
        [format!("{} t All 2", resolved.spec.run_id)]
    );
    g.title = None;
    let env = Fake::default();
    spawn(&raw(g), &env).await.unwrap();
    assert_eq!(*env.seed_sights.lock().unwrap(), ["run-1 raw Off 2"]);
}

/// A stage's mode grants what the mode cannot work without, whoever wrote the
/// graph: an output stage hands its answer back with `submit_output`, must
/// call it, and may end the run when no edge leaves it; a fan-out stage
/// starts its workers with `fan_out`. A tool list that already names the
/// tool is left as written.
#[tokio::test]
async fn a_stages_mode_grants_the_tool_it_works_through() {
    use crate::spec::graph::{FanOutDef, StageMode, ToolSelector};
    use crate::spec::names::{StageName, ToolName};
    let tool = |name: &str| ToolSelector::Tool(ToolName::new(name).unwrap());
    let names = |tools: &[ToolSelector]| -> Vec<String> {
        tools
            .iter()
            .map(|t| match t {
                ToolSelector::Tool(n) => n.to_string(),
                ToolSelector::Group(g) => format!("{g:?}"),
            })
            .collect()
    };

    let mut g = graph();
    g.stages[1].mode = StageMode::Output;
    let spec = spawn(&raw(g.clone()), &Fake::default()).await.unwrap().spec;
    let build = &spec.graph.stages[1];
    assert_eq!(names(&build.tools), ["submit_output"]);
    assert!(build.require_output);
    assert!(
        build.allow_complete,
        "nothing leaves it, so it may end the run"
    );

    // Named already: not added twice. An edge leaving it: the author routes
    // onward, and the stage does not get to end the run on its own.
    g.stages[1].tools = vec![tool("read_file"), tool("submit_output")];
    g.edges
        .push(crate::spec::graph::tests::edge("back", "build", "plan"));
    let spec = spawn(&raw(g.clone()), &Fake::default()).await.unwrap().spec;
    let build = &spec.graph.stages[1];
    assert_eq!(names(&build.tools), ["read_file", "submit_output"]);
    assert!(!build.allow_complete);

    let mut g = graph();
    g.stages[1].allow_as_worker = true;
    g.stages[0].mode = StageMode::FanOut(FanOutDef::same_graph(StageName::new("build").unwrap()));
    let spec = spawn(&raw(g), &Fake::default()).await.unwrap().spec;
    assert_eq!(names(&spec.graph.stages[0].tools), ["fan_out"]);
    assert!(
        spec.graph.stages[1].tools.is_empty(),
        "other modes grant nothing"
    );
}
