//! A resolved run spec for an agent spawned from a parsed blueprint.
//!
//! Spawning still starts from a [`Blueprint`] whose region budgets the spawn
//! has already resolved against each stage's model window. This reads that
//! blueprint as the [`RunSpec`] the rest of the runtime works from, with each
//! stage's plan taken from what the spawn resolved for it, so a system that
//! reads the spec sees exactly what the spawn decided.

use std::collections::BTreeMap;

use leviath_core::JsonDoc;

use crate::pipeline::StageInference;
use crate::spec::Blueprint;
use crate::spec::graph::{ArtifactDef, CodeRef, OutputDef, RunGraph};
use crate::spec::inputs::InputValues;
use crate::spec::issues::SpawnIssues;
use crate::spec::launch::{Delivery, LaunchPolicy, Placement, Unattended};
use crate::spec::names::{
    BlueprintName, BlueprintRef, MimePattern, ModelId, ModelRef, ProviderName, RegionName, RunId,
    ToolName,
};
use crate::spec::run_spec::{EnvFingerprint, RunSpec, SpecOrigin, StagePlan, ToolDef, ToolSource};

/// Read a spawned blueprint as a run spec.
///
/// `bp` is the blueprint as the spawn left it, with every region budget
/// resolved to tokens. `stages` and `windows` are the spawn's per-stage
/// resolution and each stage's model window, aligned with `bp.stages`. Fails
/// only when the blueprint cannot be read as a graph at all, with every field
/// that does not fit.
pub fn run_spec_from_blueprint(
    bp: &Blueprint,
    agent_id: &str,
    stages: &[StageInference],
    windows: &[usize],
) -> Result<RunSpec, SpawnIssues> {
    let graph = RunGraph::from_blueprint(bp)?;
    let plans = bp
        .stages
        .iter()
        .zip(&graph.stages)
        .zip(stages.iter().zip(windows))
        .map(|((stage, def), (inference, window))| StagePlan {
            stage: def.name.clone(),
            provider: provider(&inference.provider_name),
            model: model(&inference.model),
            context_window: clamp(*window),
            max_output_tokens: max_output_tokens(stage, bp, *window),
            fallbacks: inference
                .fallbacks
                .iter()
                .map(|e| ModelRef {
                    provider: ProviderName::new(e.provider.as_str()).ok(),
                    model: model(&e.model),
                })
                .collect(),
            tools: inference.tools.iter().filter_map(tool_def).collect(),
            output: inference.output.as_ref().map(output_def),
            region_budgets: region_budgets(bp, stage, &graph, def),
            notes: Vec::new(),
        })
        .collect();
    let origin = match BlueprintName::new(bp.name.as_str()) {
        Ok(name) => SpecOrigin::Blueprint {
            blueprint: BlueprintRef { name, digest: None },
            version: bp.version.clone(),
        },
        Err(_) => SpecOrigin::Raw,
    };
    Ok(RunSpec {
        run_id: RunId::new(agent_id).unwrap_or_else(|_| RunId::new("run").expect("a valid id")),
        origin,
        graph,
        inputs: InputValues::default(),
        stages: plans,
        seeded: BTreeMap::new(),
        code: Vec::new(),
        requested_output: None,
        requested_model: None,
        launch: LaunchPolicy {
            unattended: Unattended::Off,
            allow: Vec::new(),
            max_depth: bp
                .max_child_depth
                .map_or(u8::MAX, |d| u8::try_from(d).unwrap_or(u8::MAX)),
            seed_commands: true,
            capture_model_input: false,
        },
        placement: Placement {
            workdir: std::path::PathBuf::new(),
            parent: None,
            depth: 0,
            worker_stage: None,
        },
        delivery: Delivery::default(),
        env: EnvFingerprint::default(),
        created_at: chrono::Utc::now().timestamp(),
    })
}

/// A number of tokens as the spec stores it.
fn clamp(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A provider name the spawn resolved. A name the spec cannot hold (only a
/// hand-built test world makes one) stands in as `unnamed`.
fn provider(name: &str) -> ProviderName {
    ProviderName::new(name).unwrap_or_else(|_| ProviderName::new("unnamed").expect("a valid name"))
}

/// A model id the spawn resolved, with the same stand-in as [`provider`].
fn model(name: &str) -> ModelId {
    ModelId::new(name).unwrap_or_else(|_| ModelId::new("unnamed").expect("a valid id"))
}

/// The tokens each region the stage sees may take in it.
///
/// `def` is the stage as the graph reads it: its layout holds the same regions
/// in the same order, by their checked names.
fn region_budgets(
    bp: &Blueprint,
    stage: &crate::spec::Stage,
    graph: &RunGraph,
    def: &crate::spec::graph::StageDef,
) -> BTreeMap<RegionName, u32> {
    let layout = stage.context_layout.as_ref().unwrap_or(&bp.context_layout);
    let visible = bp.regions_visible_to(stage);
    layout
        .regions
        .iter()
        .zip(&graph.layout_for(def).regions)
        .filter(|(r, _)| visible.contains(r.name.as_str()))
        .map(|(r, d)| (d.name.clone(), clamp(r.max_tokens)))
        .collect()
}

/// The stage's reply cap in tokens, when it sets one.
fn max_output_tokens(stage: &crate::spec::Stage, bp: &Blueprint, window: usize) -> Option<u32> {
    use crate::spec::blueprint::OutputCap;
    match stage.model.output_cap().ok().flatten()? {
        OutputCap::Tokens(n) => Some(clamp(n)),
        OutputCap::WindowPercent(f) => Some(clamp((window as f64 * f).round() as usize)),
        OutputCap::RegionPercent { percent, region } => {
            let layout = stage.context_layout.as_ref().unwrap_or(&bp.context_layout);
            let budget = layout.get_region(&region)?.max_tokens;
            Some(clamp((budget as f64 * percent).round() as usize))
        }
    }
}

/// A resolved tool as the spec records it. Where it came from is not known
/// here, so it reads as built in.
fn tool_def(tool: &leviath_providers::Tool) -> Option<ToolDef> {
    Some(ToolDef {
        name: ToolName::new(tool.name.as_str()).ok()?,
        description: tool.description.clone(),
        schema: JsonDoc::new(tool.parameters.clone()),
        source: ToolSource::Builtin,
    })
}

/// A resolved output shape as the spec records it.
fn output_def(o: &leviath_core::output::OutputSpec) -> OutputDef {
    OutputDef {
        format: o.format.clone(),
        instructions: o.instructions.clone(),
        example: o.example.clone(),
        schema: o.schema.clone().map(JsonDoc::new),
        validator: o.validator.clone().map(CodeRef::File),
        on_validator_error: o.on_validator_error,
        overwrite_artifacts: o.overwrite_artifacts,
        artifacts: o
            .artifacts
            .iter()
            .filter_map(|a| {
                Some(ArtifactDef {
                    name: a.name.clone(),
                    mime_type: MimePattern::new(a.mime_type.as_str()).ok()?,
                    required: a.required,
                    description: a.description.clone(),
                })
            })
            .collect(),
    }
}

/// Specs and graphs for tests of the systems that read them.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::insert::RunSpecC;
    use crate::spec::graph::StageDef;

    /// The graph a blueprint manifest reads as.
    pub(crate) fn graph_of(manifest: &str) -> RunGraph {
        let bp = crate::spec::manifest::parse_manifest(manifest).expect("the manifest parses");
        RunGraph::from_blueprint(&bp).expect("the manifest reads as a graph")
    }

    /// A stage with every setting at its default.
    pub(crate) fn stage(name: &str) -> StageDef {
        let graph = graph_of(&format!(
            "[agent]\nname = \"t\"\n[stages.\"{name}\"]\nsystem_prompt = \"p\"\n"
        ));
        graph.stages.into_iter().next().expect("one stage")
    }

    /// A spec named `name` over `graph`, with one plan per stage giving each
    /// region of the stage's layout its fixed budget.
    pub(crate) fn spec_named(name: &str, graph: RunGraph) -> RunSpec {
        let base = crate::spec::run_spec::tests::spec();
        let plan = base.stages[0].clone();
        let stages = graph
            .stages
            .iter()
            .map(|s| StagePlan {
                stage: s.name.clone(),
                region_budgets: graph
                    .layout_for(s)
                    .regions
                    .iter()
                    .map(|r| {
                        let tokens = crate::context_setup::budget_tokens(&r.budget, 128_000);
                        (r.name.clone(), clamp(tokens))
                    })
                    .collect(),
                ..plan.clone()
            })
            .collect();
        RunSpec {
            origin: SpecOrigin::Blueprint {
                blueprint: BlueprintRef::parse(name).expect("a valid name"),
                version: "1".into(),
            },
            graph,
            stages,
            ..base
        }
    }

    /// [`spec_named`], as the component a run carries.
    pub(crate) fn spec_c(name: &str, graph: RunGraph) -> RunSpecC {
        RunSpecC(std::sync::Arc::new(spec_named(name, graph)))
    }

    /// What a spawn of `bp` places for the systems that read a run's graph:
    /// the parsed blueprint and the spec read from it, each stage on a
    /// 128k-token model.
    pub(crate) fn both(bp: Blueprint) -> (crate::pipeline::AgentBlueprint, RunSpecC) {
        let stages: Vec<StageInference> = bp
            .stages
            .iter()
            .map(|_| StageInference {
                provider_name: "script".into(),
                model: "m".into(),
                tools: vec![],
                tool_filter: None,
                fallbacks: vec![],
                output: None,
            })
            .collect();
        let windows = vec![128_000; stages.len()];
        let spec = run_spec_from_blueprint(&bp, "run", &stages, &windows)
            .expect("the blueprint reads as a spec");
        (
            crate::pipeline::AgentBlueprint(bp),
            RunSpecC(std::sync::Arc::new(spec)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::blueprint::ModelEntry;

    fn inference(provider: &str, model: &str) -> StageInference {
        StageInference {
            provider_name: provider.to_string(),
            model: model.to_string(),
            tools: vec![
                leviath_providers::Tool {
                    name: "read_file".into(),
                    description: "read".into(),
                    parameters: serde_json::json!({"type": "object"}),
                },
                leviath_providers::Tool {
                    name: "bad name".into(),
                    description: String::new(),
                    parameters: serde_json::json!({}),
                },
            ],
            tool_filter: None,
            fallbacks: vec![ModelEntry::new("other".into(), "m2".into())],
            output: Some(leviath_core::output::OutputSpec {
                format: Some("json".into()),
                validator: Some("v.rhai".into()),
                artifacts: vec![
                    leviath_core::output::ArtifactSpec {
                        name: "a.png".into(),
                        mime_type: "image/png".into(),
                        required: true,
                        description: None,
                    },
                    leviath_core::output::ArtifactSpec {
                        name: "b".into(),
                        mime_type: "not a type".into(),
                        required: false,
                        description: None,
                    },
                ],
                ..Default::default()
            }),
        }
    }

    /// Every bundled blueprint, resolved the way the spawn resolves it, reads
    /// as a spec whose plans and budgets are the spawn's own numbers.
    #[test]
    fn a_bundled_blueprint_reads_as_the_spec_the_spawn_resolved() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../leviath-cli/agents");
        let manifests: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| std::fs::read_to_string(e.unwrap().path().join("agent.leviath")).ok())
            .collect();
        let found = manifests.len();
        assert!(found >= 7, "found {found} bundled blueprints");
        for text in manifests {
            let mut bp = crate::spec::manifest::parse_manifest(&text).unwrap();
            let window = 200_000;
            bp.context_layout = bp.context_layout.resolved(window);
            let stages: Vec<StageInference> = bp
                .stages
                .iter()
                .map(|_| inference("mock", "gpt-mock"))
                .collect();
            let windows = vec![window; stages.len()];
            let spec = run_spec_from_blueprint(&bp, "run-1", &stages, &windows).unwrap();
            assert_eq!(spec.stages.len(), bp.stages.len());
            for (stage, plan) in bp.stages.iter().zip(&spec.stages) {
                assert_eq!(plan.stage.as_str(), stage.name);
                assert_eq!(plan.context_window, window as u32);
                let layout = stage.context_layout.as_ref().unwrap_or(&bp.context_layout);
                let visible = bp.regions_visible_to(stage);
                let want: BTreeMap<String, u32> = layout
                    .regions
                    .iter()
                    .filter(|r| visible.contains(r.name.as_str()))
                    .map(|r| (r.name.clone(), r.max_tokens as u32))
                    .collect();
                let got: BTreeMap<String, u32> = plan
                    .region_budgets
                    .iter()
                    .map(|(k, v)| (k.to_string(), *v))
                    .collect();
                assert_eq!(got, want);
                assert_eq!(plan.tools.len(), 1, "the unnameable tool is left out");
                assert_eq!(plan.fallbacks[0].to_string(), "other/m2");
                let output = plan.output.as_ref().unwrap();
                assert_eq!(output.artifacts.len(), 1);
                assert_eq!(output.validator, Some(CodeRef::File("v.rhai".into())));
            }
            assert_eq!(spec.run_id.as_str(), "run-1");
            assert_ne!(spec.origin, SpecOrigin::Raw);
        }
    }

    fn one_stage() -> Blueprint {
        crate::spec::manifest::parse_manifest(
            r#"
[agent]
name = "t"
version = "1"
description = ""

[context.regions]
task = { kind = "pinned", max_tokens = 1000 }

[stages.main]
system_prompt = "p"
"#,
        )
        .unwrap()
    }

    #[test]
    fn each_output_cap_resolves_to_tokens() {
        let spec_for = |cap: Option<serde_json::Value>| {
            let mut bp = one_stage();
            if let Some(cap) = cap {
                bp.stages[0]
                    .model
                    .parameters
                    .insert("max_output_tokens".into(), cap);
            }
            run_spec_from_blueprint(&bp, "r", &[inference("", "")], &[10_000])
                .unwrap()
                .stages[0]
                .clone()
        };
        let plan = spec_for(Some(serde_json::json!(500)));
        assert_eq!(plan.max_output_tokens, Some(500));
        assert_eq!(plan.provider.as_str(), "unnamed", "an empty name stands in");
        assert_eq!(plan.model.as_str(), "unnamed");
        let plan = spec_for(Some(serde_json::json!("10%")));
        assert_eq!(plan.max_output_tokens, Some(1000));
        let plan = spec_for(Some(serde_json::json!("50% of task")));
        assert_eq!(plan.max_output_tokens, Some(500));
        let plan = spec_for(Some(serde_json::json!("50% of ghost")));
        assert_eq!(
            plan.max_output_tokens, None,
            "an unknown region caps nothing"
        );
        assert_eq!(spec_for(None).max_output_tokens, None);
    }

    #[test]
    fn an_unreadable_blueprint_or_name_is_reported_or_stood_in() {
        let mut bp = one_stage();
        bp.name = " padded ".into();
        bp.max_child_depth = Some(300);
        let spec =
            run_spec_from_blueprint(&bp, "not/a/path", &[inference("p", "m")], &[1]).unwrap();
        assert_eq!(spec.origin, SpecOrigin::Raw);
        assert_eq!(spec.run_id.as_str(), "run");
        assert_eq!(spec.launch.max_depth, u8::MAX);
        bp.stages[0].available_tools = vec!["bad tool".into()];
        let issues = run_spec_from_blueprint(&bp, "r", &[inference("p", "m")], &[1]).unwrap_err();
        assert_eq!(issues.len(), 1);
    }
}
