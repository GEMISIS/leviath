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
    BlueprintName, BlueprintPath, BlueprintRef, Digest, McpServerName, ModelId, ModelRef,
    ProviderName, RegionName, RunId, StageName, ToolName,
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
    /// What its unattended setting answers without a person, as read when
    /// the run was resolved.
    pub auto_answers: AutoAnswers,
    /// Where it runs.
    pub placement: Placement,
    /// Who hears about it.
    pub delivery: Delivery,
    /// What the run relied on from this machine, so a resume can tell when
    /// that has changed.
    pub env: EnvFingerprint,
    /// When it was resolved, in unix seconds.
    pub created_at: i64,
    /// For a run converted from an earlier release, what that release listed
    /// it with where this build would list the same run otherwise. `None`
    /// for every run this build resolved.
    pub listed: Option<ListedAs>,
}

/// What an earlier release listed a run converted from it with, where this
/// build would list the same run otherwise: each as the run's own record
/// said it.
///
/// What the run recorded about itself (its model, its stages, its blueprint,
/// its tree's depth cap) is listed so for good. How it was doing (when it
/// last made progress, its working clock, whether it came to nothing) is
/// listed so while the run stands where it was converted, at step `seq`,
/// and as this build lists any run once it moves on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListedAs {
    /// The model it was listed under. `None` where its record named none: a
    /// run refused before it started, or one from a release that did not
    /// record it.
    pub model: Option<String>,
    /// How many stages it was listed with: none for a run refused before it
    /// started.
    pub num_stages: u32,
    /// The depth its tree of child runs was listed as capped at: the cap it
    /// recorded once it started a child run, and none before that.
    pub max_child_depth: u32,
    /// The revision of its blueprint it was listed under, where it named one.
    pub blueprint_digest: Option<String>,
    /// The step of its run file the run was converted at.
    pub seq: u64,
    /// When it was listed as last making progress, where its record said.
    pub last_progress_at: Option<i64>,
    /// The working clock it was listed with, where its record kept one.
    pub clock: Option<crate::state::Clock>,
    /// Whether it was listed as having stopped with nothing to show for
    /// itself.
    pub empty_output: bool,
    /// Its stage ledger as its record kept it, where this build reads the
    /// same ledger another way: whether each stage was entered, and the
    /// working clocks the stage and each of its visits kept.
    pub stages: Vec<ListedStage>,
    /// When the first point of its context history was recorded, as that
    /// release listed the history: the first record that held its window.
    /// `None` where it listed no history, for a run that kept no journal.
    pub first_point_at: Option<i64>,
}

/// One stage of a converted run's ledger, as its record kept it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListedStage {
    /// The stage, by name.
    pub stage: String,
    /// Whether the record said the run entered it. A record from a release
    /// that did not keep the answer says it did not.
    pub entered: bool,
    /// The stage's working clock, where its record kept one.
    pub clock: Option<crate::state::Clock>,
    /// Each recorded visit's working clock, in order, where its record kept
    /// one.
    pub visits: Vec<Option<crate::state::Clock>>,
}

impl ListedAs {
    /// What the run is listed with while it stands at step `seq`: all of
    /// this while it stands where it was converted, and nothing once it
    /// moves on.
    pub fn standing(listed: Option<&Self>, seq: u64) -> Option<&Self> {
        listed.filter(|l| l.seq == seq)
    }
}

impl RunSpec {
    /// A stage's plan, by name.
    pub fn stage(&self, name: &str) -> Option<&StagePlan> {
        self.stages.iter().find(|s| s.stage.as_str() == name)
    }

    /// What a fan-out worker of this run's own graph runs: the blueprint this
    /// run ran (an installed one at the revision it ran), so the files beside
    /// it (hooks, validators, scripts) are there for the worker too, or for a
    /// run whose caller wrote its graph (or whose graph was recorded from an
    /// old run), that graph.
    pub fn same_graph_source(&self) -> crate::spec::request::SpawnSource {
        use crate::spec::request::SpawnSource;
        match &self.origin {
            SpecOrigin::Blueprint { blueprint, .. } => SpawnSource::Blueprint(blueprint.clone()),
            SpecOrigin::BlueprintFile { path, .. } => SpawnSource::BlueprintFile(path.clone()),
            SpecOrigin::Raw | SpecOrigin::Recorded { .. } => {
                SpawnSource::Raw(Box::new(self.graph.clone()))
            }
        }
    }

    /// What may keep the run from ever finishing, read off its graph: the
    /// warnings a spawn answers with and every view of the run repeats.
    pub fn warnings(&self) -> crate::spec::issues::SpawnIssues {
        self.graph
            .warnings(&crate::spec::issues::SpecPath::root().field("graph"))
    }

    /// The digest the graph's reference to some code resolved to.
    pub fn code_digest(&self, code: &CodeRef) -> Option<&Digest> {
        self.code.iter().find(|(c, _)| c == code).map(|(_, d)| d)
    }
}

/// What a run answers for itself instead of asking a person.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct AutoAnswers {
    /// The model's own questions (`ask_user_*` and the like): the tools that
    /// only ask are not offered, and any asked anyway are answered for it.
    pub questions: bool,
    /// Stage checkpoints approve themselves.
    pub checkpoints: bool,
    /// Taint-gate prompts approve themselves.
    pub gate: bool,
}

impl AutoAnswers {
    /// Everything answered: a run nobody is watching at all.
    pub fn all() -> Self {
        Self {
            questions: true,
            checkpoints: true,
            gate: true,
        }
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
        /// The blueprint file it was read from, as the run's record shows
        /// it. Empty when that is not known.
        #[serde(default)]
        manifest: String,
    },
    /// A blueprint read from its directory.
    BlueprintFile {
        /// The directory.
        path: BlueprintPath,
        /// The name its manifest gives it (`[blueprint] name`).
        name: BlueprintName,
        /// The revision that ran.
        digest: Option<Digest>,
        /// Its declared version.
        version: String,
    },
    /// A graph the caller wrote.
    Raw,
    /// A run from an earlier release whose blueprint could not be read when
    /// it was converted. Its graph is what the run recorded (the stages it
    /// entered, the models they ran on, the edges it took, its regions):
    /// enough to read the run back, not to run it, so it never resumes.
    Recorded {
        /// The blueprint the run ran.
        name: BlueprintName,
        /// The blueprint file the run recorded.
        manifest: String,
        /// Why the blueprint could not be read.
        why: String,
    },
}

impl SpecOrigin {
    /// What the blueprint the run came from is called: an installed one's
    /// name, or the name the manifest of one read from a directory gives it.
    /// What its runs are listed under and what the operator's per-agent
    /// settings are looked up by. A graph its caller wrote has none.
    pub fn blueprint_name(&self) -> Option<&str> {
        match self {
            Self::Blueprint { blueprint, .. } => Some(blueprint.name.as_str()),
            Self::BlueprintFile { name, .. } | Self::Recorded { name, .. } => Some(name.as_str()),
            Self::Raw => None,
        }
    }

    /// The revision of the blueprint that ran, when one did and it is known.
    pub fn digest(&self) -> Option<&Digest> {
        match self {
            Self::Blueprint { blueprint, .. } => blueprint.digest.as_ref(),
            Self::BlueprintFile { digest, .. } => digest.as_ref(),
            Self::Raw | Self::Recorded { .. } => None,
        }
    }

    /// The blueprint file the run was read from, as its record shows it:
    /// the manifest in a directory a run named, or the file an installed or
    /// recorded one names. Empty for a graph its caller wrote, and for an
    /// installed blueprint whose file is not known.
    pub fn manifest(&self) -> String {
        match self {
            Self::Blueprint { manifest, .. } | Self::Recorded { manifest, .. } => manifest.clone(),
            Self::BlueprintFile { path, .. } => path
                .path()
                .join(leviath_core::files::BLUEPRINT_MANIFEST)
                .to_string_lossy()
                .into_owned(),
            Self::Raw => String::new(),
        }
    }

    /// Why a run from this origin can never be resumed, when it cannot.
    pub fn never_resumes(&self) -> Option<&str> {
        match self {
            Self::Recorded { why, .. } => Some(why),
            _ => None,
        }
    }
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
                manifest: String::new(),
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
            auto_answers: AutoAnswers::all(),
            placement: Placement {
                workdir: "/tmp/w".into(),
                parent: None,
                depth: 0,
                worker_stage: None,
                work_item: None,
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
            listed: None,
        }
    }

    /// What an earlier release listed a run with, every field set.
    pub(crate) fn listed() -> ListedAs {
        ListedAs {
            model: Some("mock/gpt-mock".into()),
            num_stages: 2,
            max_child_depth: 3,
            blueprint_digest: Some("ab".repeat(32)),
            seq: 4,
            last_progress_at: Some(5),
            clock: Some(crate::state::Clock {
                banked_secs: 6,
                since: Some(7),
            }),
            empty_output: true,
            first_point_at: Some(3),
            stages: vec![ListedStage {
                stage: "plan".into(),
                entered: false,
                clock: None,
                visits: vec![
                    None,
                    Some(crate::state::Clock {
                        banked_secs: 0,
                        since: None,
                    }),
                ],
            }],
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

    #[test]
    fn an_origin_names_its_blueprint_file_and_whether_it_resumes() {
        let name = BlueprintName::new("coder").unwrap();
        let recorded = SpecOrigin::Recorded {
            name: name.clone(),
            manifest: "agents/coder/agent.leviath".into(),
            why: "gone".into(),
        };
        assert_eq!(recorded.blueprint_name(), Some("coder"));
        assert_eq!(recorded.digest(), None);
        assert_eq!(recorded.manifest(), "agents/coder/agent.leviath");
        assert_eq!(recorded.never_resumes(), Some("gone"));
        assert_eq!(SpecOrigin::Raw.manifest(), "");
        assert_eq!(SpecOrigin::Raw.never_resumes(), None);
        let mut spec = spec();
        spec.origin = recorded;
        assert_eq!(
            spec.same_graph_source(),
            crate::spec::request::SpawnSource::Raw(Box::new(spec.graph.clone()))
        );
    }
}
