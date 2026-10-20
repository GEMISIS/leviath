//! What a request would run, in brief: the answer to a dry run.
//!
//! Validating a spawn resolves it the whole way without starting anything.
//! The caller gets either every [`SpawnIssue`](super::issues::SpawnIssue)
//! or a [`SpawnSummary`]: enough of the resolved [`RunSpec`] to see that it
//! is the run they meant, without the seeded context or the code.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::inputs::InputValues;
use super::issues::SpawnIssues;
use super::launch::{LaunchPolicy, Unattended};
use super::names::{ModelId, ProviderName, RunId, StageName, ToolName};
use super::run_spec::{AutoAnswers, RunSpec, SpecOrigin};

/// A resolved run, in brief.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SpawnSummary {
    /// What the run is called: its blueprint's name, or a raw graph's title.
    pub title: String,
    /// Where its graph came from.
    pub origin: SpecOrigin,
    /// The stage it starts in.
    pub entry_stage: StageName,
    /// Each stage, in the graph's order.
    pub stages: Vec<StageSummary>,
    /// The checked inputs.
    pub inputs: InputValues,
    /// What it would be trusted with.
    pub launch: LaunchPolicy,
    /// What it would answer for itself instead of asking a person.
    #[serde(default)]
    pub auto_answers: AutoAnswers,
    /// Who would answer its prompts, in a line: "approvals waived; questions
    /// still reach the user", say.
    #[serde(default)]
    pub attention: String,
    /// The directory its tools would work in.
    pub workdir: PathBuf,
    /// What may keep it from ever finishing: stages it can reach and never
    /// leave. Warnings, never refusals; empty for most runs.
    #[serde(default)]
    pub warnings: SpawnIssues,
}

/// A run that was started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Spawned {
    /// The new run's id.
    pub run_id: RunId,
    /// What may keep it from ever finishing, as [`SpawnSummary::warnings`].
    #[serde(default)]
    pub warnings: SpawnIssues,
    /// Whether this answers a request sent again under an idempotency key:
    /// the run is the one the first request started, and nothing new
    /// started.
    #[serde(default)]
    pub deduplicated: bool,
}

/// One stage of a resolved run, in brief.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StageSummary {
    /// The stage.
    pub stage: StageName,
    /// The provider that would serve it.
    pub provider: ProviderName,
    /// The model it would run on.
    pub model: ModelId,
    /// The tools it would be given, by name.
    pub tools: Vec<ToolName>,
}

impl SpawnSummary {
    /// The summary of `spec`.
    pub fn of(spec: &RunSpec) -> Self {
        let title = spec.origin.blueprint_name().map_or_else(
            || spec.graph.title.clone().unwrap_or_else(|| "run".into()),
            str::to_string,
        );
        let entry = crate::pipeline::spec_view::entry_index(&spec.graph);
        Self {
            title,
            origin: spec.origin.clone(),
            entry_stage: spec.graph.stages[entry].name.clone(),
            stages: spec
                .stages
                .iter()
                .map(|plan| StageSummary {
                    stage: plan.stage.clone(),
                    provider: plan.model.provider.clone(),
                    model: plan.model.id.clone(),
                    tools: plan.tools.iter().map(|t| t.name.clone()).collect(),
                })
                .collect(),
            inputs: spec.inputs.clone(),
            launch: spec.launch.clone(),
            auto_answers: spec.auto_answers,
            attention: attention(&spec.launch.unattended, &spec.auto_answers),
            workdir: spec.placement.workdir.clone(),
            warnings: spec.warnings(),
        }
    }
}

/// Who answers a run's prompts, in a line: its tool approvals, as its
/// unattended setting has them, and the model's own questions.
pub fn attention(unattended: &Unattended, answers: &AutoAnswers) -> String {
    let approvals = match unattended {
        Unattended::Off => return "a person answers every approval and question".to_string(),
        Unattended::All => "approvals waived".to_string(),
        Unattended::Profile(name) => format!("approvals as the yolo profile \"{name}\" says"),
    };
    let questions = match answers.questions {
        true => "the run answers its own questions",
        false => "questions still reach the user",
    };
    format!("{approvals}; {questions}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_names_each_stage_its_model_and_tools() {
        let spec = crate::spec::run_spec::tests::spec();
        let summary = SpawnSummary::of(&spec);
        assert_eq!(summary.stages.len(), spec.stages.len());
        let first = &summary.stages[0];
        assert_eq!(first.stage, spec.stages[0].stage);
        assert_eq!(first.model, spec.stages[0].model.id);
        assert_eq!(
            first.tools,
            spec.stages[0]
                .tools
                .iter()
                .map(|t| t.name.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(summary.launch, spec.launch);
        assert_eq!(summary.auto_answers, spec.auto_answers);
        assert_eq!(
            summary.attention,
            attention(&spec.launch.unattended, &spec.auto_answers)
        );
        let json = serde_json::to_string(&summary).unwrap();
        assert_eq!(
            serde_json::from_str::<SpawnSummary>(&json).unwrap(),
            summary
        );
    }

    /// The line says what goes ahead without a person, and whether the
    /// model's questions still come to one.
    #[test]
    fn attention_says_who_answers_approvals_and_questions() {
        let kept = AutoAnswers::default();
        let answered = AutoAnswers::all();
        let careful = Unattended::Profile(crate::spec::names::ProfileName::new("careful").unwrap());
        assert_eq!(
            attention(&Unattended::Off, &kept),
            "a person answers every approval and question"
        );
        assert_eq!(
            attention(&Unattended::All, &answered),
            "approvals waived; the run answers its own questions"
        );
        assert_eq!(
            attention(&Unattended::All, &kept),
            "approvals waived; questions still reach the user"
        );
        assert_eq!(
            attention(&careful, &kept),
            "approvals as the yolo profile \"careful\" says; questions still reach the user"
        );
    }

    #[test]
    fn a_raw_graph_is_summed_up_by_its_title_or_as_a_run() {
        let mut spec = crate::spec::run_spec::tests::spec();
        spec.origin = SpecOrigin::Raw;
        spec.graph.title = Some("sketch".into());
        assert_eq!(SpawnSummary::of(&spec).title, "sketch");
        spec.graph.title = None;
        assert_eq!(SpawnSummary::of(&spec).title, "run");
    }
}
