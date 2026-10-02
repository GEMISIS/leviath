use super::*;
use crate::spec::graph::{FanOutDef, StageMode, WorkerFailure, WorkerSource};
use crate::spec::inputs::{InputValue, Template};

fn slot_input(name: &str, ty: InputType, binds: Vec<InputSlot>) -> InputDecl {
    InputDecl {
        ty,
        binds,
        ..text_input(name, "task")
    }
}

fn int_at_least_one() -> InputType {
    InputType::Int {
        min: Some(1),
        max: None,
    }
}

#[tokio::test]
async fn inputs_fill_their_slots_in_the_graph() {
    let mut g = graph();
    g.stages[1].mode = StageMode::FanOut(FanOutDef {
        worker: WorkerSource::Stage(n("plan")),
        merge_stage: None,
        max_workers: Some(4),
        on_worker_failure: WorkerFailure::Continue,
        split_prompt: String::new(),
        results_region: None,
        max_items: None,
        max_attempts: None,
    });
    g.stages[0].allow_as_worker = true;
    g.stages[0].model.models = vec![model("mock/old"), model("mock/new")];
    g.inputs.extend([
        slot_input(
            "model",
            InputType::Model,
            vec![InputSlot::StageModel(n("plan"))],
        ),
        slot_input(
            "turns",
            int_at_least_one(),
            vec![InputSlot::StageMaxIterations(n("plan"))],
        ),
        slot_input(
            "workers",
            int_at_least_one(),
            vec![InputSlot::FanOutMaxWorkers(n("build"))],
        ),
        slot_input(
            "format",
            InputType::Choice {
                options: vec![n("markdown"), n("json")],
            },
            vec![InputSlot::OutputFormat],
        ),
        slot_input(
            "style",
            InputType::Text {
                multiline: false,
                min_len: None,
                max_len: None,
            },
            vec![InputSlot::OutputInstructions],
        ),
    ]);
    let request = raw(g)
        .input("model", RawInput::Text("mock/new".into()))
        .input("turns", RawInput::Int(7))
        .input("workers", RawInput::Int(2))
        .input("format", RawInput::Text("json".into()))
        .input("style", RawInput::Text("terse".into()));
    let env = Fake::default();
    let resolved = spawn(&request, &env).await.unwrap();
    let graph = &resolved.spec.graph;
    assert_eq!(
        graph.stages[0].model.models,
        [model("mock/new"), model("mock/old")]
    );
    assert_eq!(graph.stages[0].max_iterations, Some(7));
    assert!(matches!(&graph.stages[1].mode, StageMode::FanOut(f) if f.max_workers == Some(2)));
    let output = graph.output.as_ref().unwrap();
    assert_eq!(output.format.as_deref(), Some("json"));
    assert_eq!(output.instructions.as_deref(), Some("terse"));
    assert_eq!(resolved.spec.stages[0].model.as_str(), "new");
    assert_eq!(
        resolved.spec.stages[1]
            .output
            .as_ref()
            .unwrap()
            .format
            .as_deref(),
        Some("json")
    );
}

#[tokio::test]
async fn a_template_puts_several_inputs_into_one_region() {
    let mut g = graph();
    g.inputs.push(InputDecl {
        binds: vec![InputSlot::Region(RegionBinding {
            region: n("system"),
            template: Some(Template::parse("Focus on {area}; the task is {task}.").unwrap()),
        })],
        ..text_input("area", "system")
    });
    g.inputs.push(text_input("extra", "system"));
    let request = raw(g).input("area", RawInput::Text("auth".into()));
    let resolved = spawn(&request, &Fake::default()).await.unwrap();
    assert_eq!(
        resolved.spec.seeded["system"].text,
        "Focus on auth; the task is do the thing."
    );
    assert_eq!(
        resolved.spec.inputs.get("area"),
        Some(&InputValue::Text("auth".into()))
    );
}

#[tokio::test]
async fn a_required_region_left_empty_is_refused() {
    let mut g = graph();
    g.layout.regions[0].required = true;
    g.inputs.push(text_input("diff", "system"));
    let mut own = g.layout.clone();
    own.regions[0].required_message = Some("hand the reviewer a diff for {region}".into());
    own.regions.push(own.regions[0].clone());
    own.regions[2].name = n("scratch");
    g.stages[1].layout = Some(own);
    g.inputs.push(text_input("notes", "scratch"));
    let issues = spawn(&raw(g), &Fake::default()).await.unwrap_err();
    assert_eq!(
        found(&issues),
        [
            "source.raw.layout.regions[0] Missing",
            "source.raw.stages.build.layout.regions[2] Missing",
        ]
    );
    assert_eq!(
        issues.0[0].message,
        "required region \"system\" was not filled"
    );
    assert_eq!(issues.0[0].known, ["diff"]);
    assert_eq!(issues.0[1].message, "hand the reviewer a diff for scratch");
}

/// A required input left out is reported in the words of the required region
/// it fills, as 0.6.4 refused such a spawn with the region's
/// `required_message`. An input that fills no such region keeps the input
/// check's own words.
#[tokio::test]
async fn a_missing_input_says_its_regions_required_message() {
    let mut g = graph();
    g.layout.regions[0].required = true;
    g.layout.regions[0].required_message = Some("BRIEF-NEEDED for {region}".into());
    g.inputs.push(InputDecl {
        required: true,
        ..text_input("brief", "system")
    });
    g.inputs.push(InputDecl {
        required: true,
        ty: InputType::Model,
        binds: vec![InputSlot::StageModel(n("plan"))],
        ..text_input("mdl", "system")
    });
    let issues = spawn(&raw(g), &Fake::default()).await.unwrap_err();
    let said: Vec<(String, String)> = issues
        .iter()
        .map(|i| (i.path.to_string(), i.message.clone()))
        .collect();
    assert!(
        said.contains(&("inputs.brief".into(), "BRIEF-NEEDED for system".into())),
        "{said:?}"
    );
    assert!(
        said.contains(&("inputs.mdl".into(), "this input is required".into())),
        "{said:?}"
    );
}

/// An input given for a required region that fails its own check is one
/// issue, not two: the region is not also reported as unfilled. A required
/// input left out is the input check's to report, so its region is not
/// reported either; a region whose bound input was simply not given still is.
#[tokio::test]
async fn an_input_that_fails_its_check_is_not_also_an_unfilled_region() {
    let mut g = graph();
    g.layout.regions[0].required = true;
    g.inputs.push(InputDecl {
        ty: InputType::Text {
            multiline: false,
            min_len: None,
            max_len: Some(2),
        },
        ..text_input("diff", "system")
    });
    let mut own = g.layout.clone();
    own.regions.push(own.regions[0].clone());
    own.regions[2].name = n("scratch");
    own.regions.push(own.regions[0].clone());
    own.regions[3].name = n("notes");
    g.stages[1].layout = Some(own);
    g.inputs.push(text_input("scratch", "scratch"));
    g.inputs.push(InputDecl {
        required: true,
        ..text_input("notes", "notes")
    });
    let request = raw(g).input("diff", RawInput::Text("too long".into()));
    let issues = spawn(&request, &Fake::default()).await.unwrap_err();
    assert_eq!(
        found(&issues),
        [
            "inputs.diff OutOfRange",
            "inputs.notes Missing",
            "source.raw.stages.build.layout.regions[2] Missing",
        ]
    );
}

/// A required region no input binds to and no seed fills is the run's to
/// fill (the coder's `discovery`, written by its first stage), so a spawn
/// leaves it empty; one with a seed that came up empty is still refused.
#[tokio::test]
async fn a_required_region_the_run_fills_itself_is_not_judged_at_spawn() {
    let mut g = graph();
    g.layout.regions[0].required = true;
    assert!(spawn(&raw(g.clone()), &Fake::default()).await.is_ok());
    g.layout.regions[0].seed = Some(Seed::Literal("   ".into()));
    let issues = spawn(&raw(g), &Fake::default()).await.unwrap_err();
    assert_eq!(found(&issues), ["source.raw.layout.regions[0] Missing"]);
}

#[tokio::test]
async fn a_blank_task_with_nothing_else_is_refused() {
    let g = graph();
    let request = SpawnRequest::new(SpawnSource::Raw(Box::new(g.clone())));
    let issues = spawn(&request, &Fake::default()).await.unwrap_err();
    assert_eq!(found(&issues), ["inputs.task Missing"]);
    assert!(
        issues.0[0]
            .hint
            .as_deref()
            .unwrap()
            .contains("hand it a region or a file")
    );

    let mut other = g;
    other.inputs.push(text_input("diff", "system"));
    let request = SpawnRequest::new(SpawnSource::Raw(Box::new(other)))
        .input("diff", RawInput::Text("+ a line".into()));
    let resolved = spawn(&request, &Fake::default()).await.unwrap();
    assert!(!resolved.spec.seeded.contains_key("task"));
}

#[tokio::test]
async fn a_graph_without_a_task_needs_none() {
    let mut g = graph();
    g.inputs.clear();
    let request = SpawnRequest::new(SpawnSource::Raw(Box::new(g)));
    let resolved = spawn(&request, &Fake::default()).await.unwrap();
    assert!(resolved.spec.seeded.is_empty());
}

#[tokio::test]
async fn a_path_that_must_exist_is_looked_for_in_the_workdir() {
    let mut g = graph();
    g.inputs.push(InputDecl {
        ty: InputType::Path {
            kind: PathKind::File,
            must_exist: true,
        },
        ..text_input("target", "system")
    });
    let request = raw(g).input("target", RawInput::Text("src/lib.rs".into()));
    let issues = spawn(&request, &Fake::default()).await.unwrap_err();
    assert_eq!(found(&issues), ["inputs.target Unresolvable"]);
    assert!(issues.0[0].hint.as_deref().unwrap().contains("/work"));

    let env = Fake {
        existing: ["src/lib.rs".to_string()].into(),
        ..Fake::default()
    };
    let resolved = spawn(&request, &env).await.unwrap();
    assert_eq!(resolved.spec.seeded["system"].text, "src/lib.rs");

    let env = Fake {
        workdir: Err("gone".into()),
        ..Fake::default()
    };
    let issues = spawn(&request, &env).await.unwrap_err();
    assert_eq!(
        found(&issues),
        ["workdir Unresolvable"],
        "no workdir, nothing to look in"
    );
}

/// Something of the other kind at the path is named for what it is: a file
/// given for a directory input is not a missing path, and neither is a
/// directory given for a file.
#[tokio::test]
async fn a_path_of_the_wrong_kind_says_what_is_there() {
    let request = |kind: PathKind, given: &str| {
        let mut g = graph();
        g.inputs.push(InputDecl {
            ty: InputType::Path {
                kind,
                must_exist: true,
            },
            ..text_input("target", "system")
        });
        raw(g).input("target", RawInput::Text(given.into()))
    };
    let env = Fake {
        existing: ["notes.txt".to_string(), "src/".to_string()].into(),
        ..Fake::default()
    };
    let issues = spawn(&request(PathKind::Dir, "notes.txt"), &env)
        .await
        .unwrap_err();
    assert_eq!(found(&issues), ["inputs.target WrongType"]);
    assert_eq!(issues.0[0].message, "this path is a file, not a directory");
    let issues = spawn(&request(PathKind::File, "src"), &env)
        .await
        .unwrap_err();
    assert_eq!(issues.0[0].message, "this path is a directory, not a file");
    let issues = spawn(&request(PathKind::Any, "nope"), &env)
        .await
        .unwrap_err();
    assert_eq!(found(&issues), ["inputs.target Unresolvable"]);
    assert!(spawn(&request(PathKind::Any, "src"), &env).await.is_ok());
}

/// A `blueprint` input is held to what its type says: a blueprint this
/// machine has installed. A name nothing installs is refused at its input,
/// at a check as at a spawn.
#[tokio::test]
async fn a_blueprint_input_must_name_an_installed_blueprint() {
    let mut g = graph();
    g.inputs.push(InputDecl {
        ty: InputType::Blueprint,
        ..text_input("helper", "system")
    });
    let request = raw(g).input("helper", RawInput::Text("no-such-bp".into()));
    let issues = spawn(&request, &Fake::default()).await.unwrap_err();
    assert_eq!(found(&issues), ["inputs.helper Unknown"]);
    assert_eq!(issues.0[0].message, "no such blueprint");
    let mut env = Fake::default();
    env.blueprints.insert(
        "no-such-bp".into(),
        crate::spec::env::LoadedBlueprint {
            graph: graph(),
            reference: crate::spec::names::BlueprintRef::parse("no-such-bp").unwrap(),
            version: "1.0.0".into(),
            base_dir: std::path::PathBuf::from("/agents/no-such-bp"),
        },
    );
    assert!(
        spawn(&request, &env).await.is_ok(),
        "an installed one passes"
    );
}

/// A missing piece that is not an input keeps the words it was found with.
#[tokio::test]
async fn only_a_missing_input_takes_a_regions_words() {
    let mut g = graph();
    g.layout.regions[0].required = true;
    g.layout.regions[0].required_message = Some("NEVER SAID".into());
    g.stages.clear();
    let issues = spawn(&raw(g), &Fake::default()).await.unwrap_err();
    let missing: Vec<&str> = issues
        .iter()
        .filter(|i| i.code == IssueCode::Missing)
        .map(|i| i.message.as_str())
        .collect();
    assert!(
        missing.contains(&"a run needs at least one stage"),
        "{missing:?}"
    );
    assert!(!missing.contains(&"NEVER SAID"), "{missing:?}");
}
