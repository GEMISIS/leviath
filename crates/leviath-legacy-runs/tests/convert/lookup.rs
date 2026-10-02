//! What a converted run's stages run on and offer: the model the old run
//! would have used, and the window and tools this machine gives it.

use std::path::Path;
use std::sync::Mutex;

use leviath_core::JsonDoc;
use leviath_legacy_runs::{ConvertEnv, StageLookup, convert, graph};
use leviath_runtime::spec::env::{CodeFiles, ModelPlan, StageTools};
use leviath_runtime::spec::graph::{CodeRef, RunGraph, StageDef};
use leviath_runtime::spec::names::{Digest, ModelId, ModelRef, ProviderName, ToolName};
use leviath_runtime::spec::run_spec::{ToolDef, ToolSource};
use serde_json::json;

use crate::common::{Run, RunFile, fixtures_dir};

/// Two stages that name no model, the second one never reached, with a
/// relative reply cap and an output shape.
const TWO: &str = r#"
[agent]
name = "probe"
version = "0.1.0"
description = "Two stages, no models"
entry_stage = "main"

[context.regions]
task = { kind = "pinned", max_tokens = 2000, required = true, seed = "task" }

[stages.main]
mode = "autonomous"
description = "First"
system_prompt = "first"
max_iterations = 4
available_tools = ["shell", "ask_user_text", "submit_output", "probe_tool"]

[stages.main.model.parameters]
max_output_tokens = "10% of task"

[stages.review]
mode = "autonomous"
description = "Second"
system_prompt = "second"
max_iterations = 2
available_tools = ["submit_output", "ask_user_text"]
required_tools = ["ask_user_text"]

[stages.review.model.parameters]
max_output_tokens = "25%"

[stages.review.output]
format = "markdown"
instructions = "Two sentences."

[stages.review.output.schema]
type = "object"

[[stages.review.output.artifacts]]
name = "chart"
type = "image/png"
"#;

fn two_stage_run() -> Run {
    let run = Run::fixture("finished");
    run.write("blueprint.leviath", TWO);
    run
}

fn tool(name: &str, source: ToolSource) -> ToolDef {
    ToolDef {
        name: ToolName::new(name).unwrap(),
        description: format!("the {name} tool"),
        schema: JsonDoc::new(json!({"type": "object"})),
        source,
    }
}

/// A machine that answers every stage, or refuses every question.
#[derive(Default)]
struct Machine {
    refuse: bool,
    asked: Mutex<Vec<(String, Option<ModelRef>, usize)>>,
}

impl StageLookup for Machine {
    fn model(
        &self,
        graph: &RunGraph,
        stage: &StageDef,
        requested: Option<&ModelRef>,
    ) -> Result<ModelPlan, String> {
        if self.refuse {
            return Err("no such provider here".into());
        }
        self.asked.lock().unwrap().push((
            stage.name.to_string(),
            requested.cloned(),
            graph.stages.len(),
        ));
        let chosen = requested.cloned().unwrap_or(ModelRef {
            provider: Some(ProviderName::new("openai").unwrap()),
            model: ModelId::new("gpt-default").unwrap(),
        });
        Ok(ModelPlan {
            provider: chosen.provider.unwrap(),
            model: chosen.model,
            context_window: 200_000,
            max_output_tokens: 8_000,
            fallbacks: Vec::new(),
            notes: vec!["chosen on this machine".into()],
        })
    }

    fn tools(
        &self,
        _graph: &RunGraph,
        stage: &StageDef,
        code: &CodeFiles,
        base: Option<&Path>,
        workdir: Option<&Path>,
    ) -> Result<StageTools, String> {
        if self.refuse {
            return Err("the catalog did not load".into());
        }
        assert!(code.is_empty(), "the probe runs no code of its own");
        assert!(base.is_some_and(|b| b.ends_with("agents/probe")));
        assert!(workdir.is_some());
        let script = b"fn run(args) { \"ok\" }".to_vec();
        let mut tools = vec![
            tool("shell", ToolSource::Builtin),
            tool("ask_user_text", ToolSource::Builtin),
            tool("submit_output", ToolSource::StageControl),
        ];
        // Every stage finds the same script tool, which the file holds once.
        tools.push(tool("probe_tool", ToolSource::Script(Digest::of(&script))));
        let found = vec![(CodeRef::File("tools/probe.rhai".into()), script)];
        assert!(!stage.name.as_str().is_empty());
        Ok(StageTools { tools, code: found })
    }
}

fn convert_on(run: &Run, machine: &Machine) -> (leviath_legacy_runs::ConvertReport, RunFile) {
    let env = ConvertEnv {
        agents_dir: Some(fixtures_dir().join("agents")),
        stages: Some(machine),
    };
    let report = convert(&run.dir, &env).unwrap();
    (report, RunFile::read(&run.path("run.lvr")))
}

/// A stage the old run never reached runs on the model the run was launched
/// with, not on a provider called `unknown`.
#[test]
fn a_stage_the_run_never_reached_runs_on_the_model_it_was_launched_with() {
    let run = two_stage_run();
    run.meta(|m| m.model_override = Some("anthropic/claude-x".into()));
    let (report, file) = run.converted();
    for name in ["main", "review"] {
        let stage = file.spec.stage(name).unwrap();
        assert_eq!(stage.provider.as_str(), "anthropic", "{name}");
        assert_eq!(stage.model.as_str(), "claude-x", "{name}");
        assert!(report.defaulted(&format!("stages.{name}.model")).is_none());
    }
}

/// Without a launch model, a stage the run reached runs on the model its
/// ledger recorded, and one it never reached on its own.
#[test]
fn without_a_launch_model_a_stage_runs_on_the_model_the_run_recorded() {
    let run = two_stage_run();
    let (report, file) = run.converted();
    for (name, provider, model) in [
        ("main", "openai", "gpt-mock"),
        ("review", "anthropic", "claude-sonnet-4-6"),
    ] {
        let stage = file.spec.stage(name).unwrap();
        assert_eq!(stage.provider.as_str(), provider, "{name}");
        assert_eq!(stage.model.as_str(), model, "{name}");
        assert!(report.defaulted(&format!("stages.{name}.model")).is_none());
        assert!(stage.tools.is_empty());
    }
    assert!(report.defaulted("stages.*.tools").is_some());
    assert!(
        report
            .defaulted("stages.review.max_output_tokens")
            .is_some()
    );
}

/// The machine gives each stage its model's window and the tools the old run
/// offered, with the script tools' code carried in the file, as a new run's.
#[test]
fn a_lookup_gives_each_stage_its_window_and_its_tools() {
    let run = two_stage_run();
    run.meta(|m| m.model_override = Some("openai/gpt-mock".into()));
    let machine = Machine::default();
    let (report, file) = convert_on(&run, &machine);
    let asked = machine.asked.lock().unwrap().clone();
    let launched = ModelRef::parse("openai/gpt-mock").unwrap();
    assert_eq!(
        asked,
        vec![
            ("main".into(), Some(launched.clone()), 2),
            ("review".into(), Some(launched), 2)
        ]
    );
    let main = file.spec.stage("main").unwrap();
    assert_eq!(main.context_window, 200_000);
    assert!(main.notes.iter().any(|n| n == "chosen on this machine"));
    // A run nobody answers is not offered the tools that only ask a person.
    let names: Vec<&str> = main.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["shell", "submit_output", "probe_tool"]);
    assert_eq!(main.max_output_tokens, Some(200), "10% of the task region");
    let review = file.spec.stage("review").unwrap();
    // A stage that requires a tool that asks keeps it.
    let names: Vec<&str> = review.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        ["shell", "ask_user_text", "submit_output", "probe_tool"]
    );
    assert_eq!(file.spec.code.len(), 1);
    assert_eq!(file.code.len(), 1);
    assert_eq!(
        review.max_output_tokens,
        Some(8_000),
        "25% capped at the model's"
    );
    let submit = review
        .tools
        .iter()
        .find(|t| t.name.as_str() == "submit_output");
    assert_ne!(submit.unwrap().description, "the submit_output tool");
    let probe = CodeRef::File("tools/probe.rhai".into());
    let digest = file
        .spec
        .code_digest(&probe)
        .expect("the script tool's code");
    assert!(file.code.iter().any(|(d, _)| d == digest));
    for field in [
        "stages.*.tools",
        "stages.main.context_window",
        "stages.main.tools",
        "stages.review.max_output_tokens",
    ] {
        assert!(report.defaulted(field).is_none(), "{field}");
    }
}

/// A run a person answers keeps the tools that ask one.
#[test]
fn an_attended_run_keeps_the_tools_that_ask_a_person() {
    let run = two_stage_run();
    run.meta(|m| m.yolo = false);
    let (_, file) = convert_on(&run, &Machine::default());
    let main = file.spec.stage("main").unwrap();
    assert!(
        main.tools
            .iter()
            .any(|t| t.name.as_str() == "ask_user_text")
    );
}

/// A machine that cannot answer leaves the model the run recorded and says
/// why, and the stage gets no tools.
#[test]
fn a_lookup_that_cannot_answer_leaves_the_recorded_model_and_says_why() {
    let run = two_stage_run();
    let machine = Machine {
        refuse: true,
        ..Machine::default()
    };
    let (report, file) = convert_on(&run, &machine);
    let main = file.spec.stage("main").unwrap();
    assert_eq!(main.model.as_str(), "gpt-mock");
    assert!(main.tools.is_empty());
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("stages.main.model could not be looked up"))
    );
    assert!(report.defaulted("stages.main.tools").is_some());
    assert!(report.defaulted("stages.main.context_window").is_some());
    assert!(report.defaulted("stages.*.tools").is_none());
}

/// A host reads an old run's graph before converting it.
#[test]
fn the_graph_of_an_old_run_reads_without_converting_it() {
    let run = two_stage_run();
    let env = ConvertEnv::default();
    let read = graph(&run.dir, &env).unwrap();
    let names: Vec<&str> = read.stages.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["main", "review"]);
    assert!(leviath_legacy_runs::is_legacy(&run.dir));
    assert!(format!("{env:?}").contains("stages: false"));
    // A directory that is no run has no graph.
    assert!(graph(&run.dir.join("stages"), &env).is_err());
}
