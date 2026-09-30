//! A resolved run: the request, decided against this machine.
//!
//! A [`RunSpec`] is what the resolver makes from a [`SpawnRequest`] once it
//! has asked the machine everything it needs to: which provider serves each
//! stage, which tools each stage really gets, what every spawn-time seed
//! produced, and the code the run uses. Nothing in it is looked up again.
//! It is the run file's first frame and the starting point of the run, and
//! inserting it into the world is a placement with no decisions left in it.
//!
//! [`SpawnRequest`]: super::request::SpawnRequest

use std::collections::BTreeMap;

use leviath_core::JsonDoc;
use serde::{Deserialize, Serialize};

use super::graph::{CodeRef, OutputDef, RunGraph};
use super::inputs::InputValues;
use super::launch::{Delivery, LaunchPolicy, Placement};
use super::names::{
    BlueprintRef, Digest, McpServerName, ModelId, ModelRef, ProviderName, RegionName, RunId,
    StageName, ToolName,
};
use crate::state::context::PartState;

/// A run, fully resolved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RunSpec {
    /// The run's id.
    pub run_id: RunId,
    /// Where the graph came from.
    pub origin: SpecOrigin,
    /// The graph, with the request's input slots applied.
    pub graph: RunGraph,
    /// The checked inputs.
    pub inputs: InputValues,
    /// One plan per stage, in the graph's stage order.
    pub stages: Vec<StagePlan>,
    /// What each region holds at spawn: seeds and bound inputs, resolved.
    pub seeded: BTreeMap<RegionName, SeededContent>,
    /// The code the run uses, by the reference the graph makes to it.
    pub code: Vec<(CodeRef, Digest)>,
    /// The output shape the caller asked for, as asked.
    pub requested_output: Option<OutputDef>,
    /// The model the caller asked for, as asked.
    pub requested_model: Option<ModelRef>,
    /// What the run is trusted with.
    pub launch: LaunchPolicy,
    /// Where it runs.
    pub placement: Placement,
    /// Who hears about it.
    pub delivery: Delivery,
    /// What the run relied on from this machine, so a resume can tell when
    /// that has changed.
    pub env: EnvFingerprint,
    /// When it was resolved, in unix seconds.
    pub created_at: i64,
}

impl RunSpec {
    /// A stage's plan, by name.
    pub fn stage(&self, name: &str) -> Option<&StagePlan> {
        self.stages.iter().find(|s| s.stage.as_str() == name)
    }

    /// The digest the graph's reference to some code resolved to.
    pub fn code_digest(&self, code: &CodeRef) -> Option<&Digest> {
        self.code.iter().find(|(c, _)| c == code).map(|(_, d)| d)
    }
}

/// Where a run's graph came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum SpecOrigin {
    /// An installed blueprint.
    Blueprint {
        /// The blueprint, pinned to the revision that ran.
        blueprint: BlueprintRef,
        /// Its declared version.
        version: String,
    },
    /// A graph the caller wrote.
    Raw,
}

/// One stage, decided.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StagePlan {
    /// The stage.
    pub stage: StageName,
    /// The provider that serves it.
    pub provider: ProviderName,
    /// The model it runs.
    pub model: ModelId,
    /// That model's context window, in tokens.
    pub context_window: u32,
    /// The cap on one reply, in tokens, when there is one.
    pub max_output_tokens: Option<u32>,
    /// Where to go if the provider fails, best first.
    pub fallbacks: Vec<ModelRef>,
    /// The tools it gets.
    pub tools: Vec<ToolDef>,
    /// The final-output shape it asks for, with the caller's request applied.
    pub output: Option<OutputDef>,
    /// Each region's budget in this stage, in tokens.
    pub region_budgets: BTreeMap<RegionName, u32>,
    /// Lines worth logging about how the stage was decided.
    pub notes: Vec<String>,
}

/// A tool as a stage gets it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolDef {
    /// Its name, as the model calls it.
    pub name: ToolName,
    /// What it does, as the model reads it.
    pub description: String,
    /// The JSON Schema of its arguments.
    pub schema: JsonDoc,
    /// Where it comes from.
    pub source: ToolSource,
}

/// Where a tool comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum ToolSource {
    /// Compiled into Leviath.
    Builtin,
    /// A child-run tool.
    Subagent,
    /// A stage-control tool the engine handles itself.
    StageControl,
    /// A script tool, by the digest of its code.
    Script(Digest),
    /// An MCP server's tool, by the server and the tool's own name.
    Mcp {
        /// The server.
        server: McpServerName,
        /// The tool's name on that server.
        tool: String,
    },
}

/// What a region holds at spawn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SeededContent {
    /// Its text.
    pub text: String,
    /// Its parts.
    pub parts: Vec<PartState>,
}

/// What a run relied on from its machine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EnvFingerprint {
    /// Each provider the run uses, by a digest of its configuration (never
    /// its credentials).
    pub providers: BTreeMap<ProviderName, Digest>,
    /// Each MCP server the run uses, by a digest of its tool list.
    pub mcp_servers: BTreeMap<McpServerName, Digest>,
    /// The Leviath version that resolved it.
    pub leviath_version: String,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::spec::launch::{Callback, Secret, Unattended};
    use crate::spec::names::HttpUrl;

    /// A spec with every field set, for codec and insertion tests.
    pub(crate) fn spec() -> RunSpec {
        let graph = crate::spec::graph::tests::minimal();
        let d = Digest::of(b"code");
        RunSpec {
            run_id: RunId::new("t-1").unwrap(),
            origin: SpecOrigin::Blueprint {
                blueprint: BlueprintRef::parse(&format!("coder@{d}")).unwrap(),
                version: "1.0.0".into(),
            },
            stages: vec![StagePlan {
                stage: StageName::new("plan").unwrap(),
                provider: ProviderName::new("mock").unwrap(),
                model: ModelId::new("gpt-mock").unwrap(),
                context_window: 128_000,
                max_output_tokens: Some(8000),
                fallbacks: vec![ModelRef::parse("other/m").unwrap()],
                tools: vec![
                    ToolDef {
                        name: ToolName::new("read_file").unwrap(),
                        description: "read".into(),
                        schema: JsonDoc::new(serde_json::json!({"type": "object"})),
                        source: ToolSource::Builtin,
                    },
                    ToolDef {
                        name: ToolName::new("gh__search").unwrap(),
                        description: "s".into(),
                        schema: JsonDoc::default(),
                        source: ToolSource::Mcp {
                            server: McpServerName::new("gh").unwrap(),
                            tool: "search".into(),
                        },
                    },
                    ToolDef {
                        name: ToolName::new("mine").unwrap(),
                        description: "m".into(),
                        schema: JsonDoc::default(),
                        source: ToolSource::Script(d.clone()),
                    },
                ],
                output: Some(OutputDef::default()),
                region_budgets: [(RegionName::new("task").unwrap(), 500)].into(),
                notes: vec!["moved".into()],
            }],
            graph,
            inputs: InputValues::default(),
            seeded: [(
                RegionName::new("task").unwrap(),
                SeededContent {
                    text: "do it".into(),
                    parts: vec![],
                },
            )]
            .into(),
            code: vec![(CodeRef::File("hooks/enter.rhai".into()), d.clone())],
            requested_output: None,
            requested_model: Some(ModelRef::parse("mock/gpt-mock").unwrap()),
            launch: LaunchPolicy {
                unattended: Unattended::All,
                allow: vec![],
                max_depth: 2,
                seed_commands: true,
                capture_model_input: false,
            },
            placement: Placement {
                workdir: "/tmp/w".into(),
                parent: None,
                depth: 0,
                worker_stage: None,
            },
            delivery: Delivery {
                callback: Some(Callback {
                    url: HttpUrl::new("https://x.dev/h").unwrap(),
                    secret: Some(Secret::new("s")),
                }),
                metadata: [("team".to_string(), "a".to_string())].into(),
            },
            env: EnvFingerprint {
                providers: [(ProviderName::new("mock").unwrap(), d.clone())].into(),
                mcp_servers: [(McpServerName::new("gh").unwrap(), d)].into(),
                leviath_version: "0.6.4".into(),
            },
            created_at: 1,
        }
    }

    #[test]
    fn a_spec_survives_the_binary_codec_and_answers_lookups() {
        let s = spec();
        let bin = postcard::to_stdvec(&s).unwrap();
        assert_eq!(postcard::from_bytes::<RunSpec>(&bin).unwrap(), s);
        assert_eq!(s.stage("plan").map(|p| p.model.as_str()), Some("gpt-mock"));
        assert!(s.stage("nope").is_none());
        assert_eq!(
            s.code_digest(&CodeRef::File("hooks/enter.rhai".into())),
            Some(&Digest::of(b"code"))
        );
        assert_eq!(s.code_digest(&CodeRef::Inline("x".into())), None);
        let raw = RunSpec {
            origin: SpecOrigin::Raw,
            ..s
        };
        assert_eq!(
            postcard::from_bytes::<RunSpec>(&postcard::to_stdvec(&raw).unwrap()).unwrap(),
            raw
        );
    }
}
