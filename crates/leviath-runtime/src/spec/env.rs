//! What resolving and binding a run asks of the machine.
//!
//! The runtime decides how a run is resolved; the host (the daemon, or an
//! embedder) knows what this machine has. [`ResolveEnv`] is every question
//! resolution asks, and [`BindEnv`] is every live handle a resolved run needs.
//! Neither decides anything about the run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use bevy_ecs::bundle::Bundle;
use bevy_ecs::world::EntityWorldMut;

use leviath_core::mime::MimeRegistry;

use super::graph::{CodeRef, DependencyDef, MimeRows, NudgeDef, RunGraph, Seed, StageDef};
use super::inputs::PathKind;
use super::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use super::launch::{LaunchPolicy, Unattended};
use super::names::{
    BlueprintPath, BlueprintRef, Digest, McpServerName, MimePattern, ModelId, ModelRef,
    ProviderName, RunId, StageName, WorkdirPath,
};
use super::run_spec::{AutoAnswers, RunSpec, SeededContent, ToolDef};

/// Who is asking for a run.
#[derive(Debug, Clone, PartialEq)]
pub enum Caller {
    /// A person or program: the run is the top of its tree.
    TopLevel,
    /// A running run starting a child.
    Child {
        /// The parent.
        parent: RunId,
        /// The parent's policy, which the child's is narrowed against.
        policy: LaunchPolicy,
        /// The parent's depth.
        depth: u8,
    },
    /// A fan-out starting a worker.
    Worker {
        /// The run that fanned out.
        parent: RunId,
        /// The parent's policy.
        policy: LaunchPolicy,
        /// The parent's depth.
        depth: u8,
        /// The stage the worker runs, for a same-graph worker.
        stage: Option<StageName>,
        /// The id of the work item the worker runs.
        item: String,
    },
}

impl Caller {
    /// The work item a fan-out worker runs. `None` for any other caller.
    pub fn work_item(&self) -> Option<&str> {
        match self {
            Self::Worker { item, .. } => Some(item),
            Self::TopLevel | Self::Child { .. } => None,
        }
    }
}

/// An installed blueprint, loaded.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedBlueprint {
    /// Its graph.
    pub graph: RunGraph,
    /// The blueprint, pinned to the revision loaded.
    pub reference: BlueprintRef,
    /// Its declared version.
    pub version: String,
    /// The directory its files (code, prompts) are read from.
    pub base_dir: PathBuf,
}

/// The operator's limits and defaults for spawning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnLimits {
    /// Child-run depth for a run that does not say.
    pub default_max_depth: u8,
    /// Whether seeds that run a shell command may run at all.
    pub seed_commands_allowed: bool,
    /// The largest attachment accepted, in bytes.
    pub max_attachment_bytes: u64,
    /// The iteration ceiling for a stage that sets none. A stage that writes
    /// `0` asked for no ceiling, and does not get to opt out of this one.
    pub default_max_iterations: Option<u32>,
    /// What the operator wants where a graph leaves a setting open.
    pub defaults: OperatorDefaults,
}

/// The operator's settings a graph may leave open, filled into the graph when
/// it is resolved so the run's spec carries them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorDefaults {
    /// The batch-tool-calls hint.
    pub batch_tool_hint: bool,
    /// The platform shell hint.
    pub shell_hint: bool,
    /// The empty-response nudge, field by field.
    pub nudge: NudgeDef,
    /// Whether taint tracking is on. A graph can turn it on, never off.
    pub taint_tracking: bool,
    /// Whether every run records the requests it sends its model.
    pub capture_model_input: bool,
}

impl Default for OperatorDefaults {
    /// Both hints on, the nudge left to the engine, taint tracking and input
    /// capture off: what a machine with no settings of its own gets.
    fn default() -> Self {
        Self {
            batch_tool_hint: true,
            shell_hint: true,
            nudge: NudgeDef::default(),
            taint_tracking: false,
            capture_model_input: false,
        }
    }
}

/// The model a stage runs on, chosen.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelPlan {
    /// The provider.
    pub provider: ProviderName,
    /// The model.
    pub model: ModelId,
    /// Its context window, in tokens.
    pub context_window: u32,
    /// The most it writes in one reply, in tokens.
    pub max_output_tokens: u32,
    /// Where to go if the provider fails, best first.
    pub fallbacks: Vec<ModelRef>,
    /// Lines worth logging about the choice.
    pub notes: Vec<String>,
}

/// What some code is for, so it is checked for the entry points that use needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeUse {
    /// A stage hook.
    Hook,
    /// An output validator.
    Validator,
    /// A custom region.
    Region,
    /// A region seed.
    Seed,
    /// A mime type check.
    MimeCheck,
    /// A dependency check.
    DependencyCheck,
    /// A dependency's install script.
    Install,
    /// A script tool.
    Tool,
}

/// What a seed can see while it runs.
#[derive(Debug, Clone, Copy)]
pub struct SeedCx<'a> {
    /// The run's id.
    pub run_id: &'a RunId,
    /// The name the run's permissions and grants are looked up under.
    pub agent: &'a str,
    /// The run's graph, with its permissions, safe commands and sandbox.
    pub graph: &'a RunGraph,
    /// What the run is trusted with: its `allow` list and unattended setting.
    pub launch: &'a LaunchPolicy,
    /// The run's workdir.
    pub workdir: &'a Path,
    /// The directory of the blueprint the run came from, which a seed path
    /// written `blueprint:<path>` reads from. `None` for a graph its caller
    /// wrote.
    pub blueprint_dir: Option<&'a Path>,
    /// Whether shell-command seeds may run.
    pub commands_allowed: bool,
    /// The code the run holds, by digest, already read.
    pub code: &'a CodeFiles,
    /// Each reference the graph makes to code, and the digest it read as.
    pub code_refs: &'a [(CodeRef, Digest)],
    /// The run's checked inputs.
    pub inputs: &'a super::inputs::InputValues,
}

impl SeedCx<'_> {
    /// The bytes some code the graph names read as.
    pub fn code_of(&self, code: &CodeRef) -> Option<&[u8]> {
        self.code_refs
            .iter()
            .find(|(c, _)| c == code)
            .and_then(|(_, digest)| self.code.get(digest))
            .map(Vec::as_slice)
    }
}

/// Code read for a run, by digest.
pub type CodeFiles = BTreeMap<Digest, Vec<u8>>;

/// The tools a stage gets, and the code of any script tool among them that the
/// run did not already hold (a global script tool, say), so the run file
/// carries every byte it runs.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StageTools {
    /// The tools, each with its schema.
    pub tools: Vec<ToolDef>,
    /// Script tool code the host found, by the reference it is recorded under.
    pub code: Vec<(CodeRef, Vec<u8>)>,
}

impl From<Vec<ToolDef>> for StageTools {
    fn from(tools: Vec<ToolDef>) -> Self {
        Self {
            tools,
            code: Vec::new(),
        }
    }
}

/// Every question resolving a run asks of the machine.
#[async_trait]
pub trait ResolveEnv: Send + Sync {
    /// Load an installed blueprint.
    async fn blueprint(&self, reference: &BlueprintRef) -> Result<LoadedBlueprint, SpawnIssue>;
    /// Load the blueprint in a directory on this machine. A host that reads
    /// no blueprints from paths keeps the refusal this answers with.
    async fn blueprint_file(&self, path: &BlueprintPath) -> Result<LoadedBlueprint, SpawnIssue> {
        Err(SpawnIssue::new(
            SpecPath::root(),
            IssueCode::NotAllowed,
            format!("this host reads no blueprint from a path, so not '{path}'"),
        ))
    }
    /// The operator's limits and defaults.
    fn limits(&self) -> SpawnLimits;
    /// A new run id for a run with this title.
    fn new_run_id(&self, title: &str) -> RunId;
    /// The workdir a request asked for, checked, or the default when it asked
    /// for none.
    fn workdir(&self, requested: Option<&Path>) -> Result<PathBuf, String>;
    /// Whether `path` names something of `kind` inside `workdir`.
    fn path_exists(&self, workdir: &Path, path: &WorkdirPath, kind: PathKind) -> bool;
    /// Choose a stage's provider and model. `requested` is the caller's
    /// model, for a stage that allows one. An issue's path is relative to the
    /// stage's `model`, usually the root.
    async fn model(
        &self,
        stage: &StageDef,
        requested: Option<&ModelRef>,
    ) -> Result<ModelPlan, SpawnIssue>;
    /// Whether the compaction model may be sent the run's context here: a
    /// model this machine would not run under its retention rules is refused
    /// for summaries as it is for stages.
    fn compaction_model(&self, model: &ModelRef) -> Result<(), String>;
    /// What an unattended setting leaves to a person. `All` answers
    /// everything and `Off` nothing; a named profile is the host's to read,
    /// and a host with no profiles refuses every name.
    fn auto_answers(&self, unattended: &Unattended) -> Result<AutoAnswers, Box<SpawnIssue>> {
        match unattended {
            Unattended::Off => Ok(AutoAnswers::default()),
            Unattended::All => Ok(AutoAnswers::all()),
            Unattended::Profile(name) => Err(SpawnIssue::new(
                SpecPath::root(),
                IssueCode::Unresolvable,
                format!("no yolo profile named \"{name}\": this host has no profiles"),
            )
            .hint("run unattended with `all`, or attended")
            .into()),
        }
    }
    /// The tools a stage gets, each with its schema. `code` holds the run's
    /// code already read, for script tools, `base` is the blueprint's
    /// directory, whose own script tools the stage may use, and `workdir` is
    /// the run's, whose `tools/` a graph that looks at its tools again during
    /// the run is given from the start. An issue's path is relative to the
    /// stage: `tools[1]`, `required_tools[0]`.
    async fn tools(
        &self,
        graph: &RunGraph,
        stage: &StageDef,
        code: &CodeFiles,
        base: Option<&Path>,
        workdir: Option<&Path>,
    ) -> Result<StageTools, SpawnIssues>;
    /// Read code the graph names. `base` is the blueprint's directory, when
    /// the graph came from one.
    async fn code(&self, code: &CodeRef, base: Option<&Path>) -> Result<Vec<u8>, String>;
    /// Check that code compiles and has what `used_as` calls.
    fn check_code(&self, code: &[u8], used_as: CodeUse) -> Result<(), String>;
    /// Run a spawn-time seed.
    async fn seed(&self, seed: &Seed, cx: SeedCx<'_>) -> Result<SeededContent, String>;
    /// The registry a run types its bytes by: this machine's rows with the
    /// graph's own `rows` on top.
    fn mime_registry(&self, rows: &MimeRows) -> Result<MimeRegistry, String>;
    /// The mime type of attached bytes by `registry`: the declared one if they
    /// match it, else sniffed from the bytes and the name.
    fn sniff(
        &self,
        registry: &MimeRegistry,
        name: &str,
        bytes: &[u8],
        declared: Option<&MimePattern>,
    ) -> Result<String, String>;
    /// Whether a dependency is met. `code` is the check's code, read from
    /// the run's own copy, for a dependency judged by code.
    async fn dependency(
        &self,
        dependency: &DependencyDef,
        code: Option<&[u8]>,
    ) -> Result<(), String>;
    /// A digest of a provider's configuration, credentials left out.
    fn provider_fingerprint(&self, provider: &ProviderName) -> Option<Digest>;
    /// A digest of an MCP server's tool list.
    fn mcp_fingerprint(&self, server: &McpServerName) -> Option<Digest>;
}

/// The live handles a resolved run needs.
#[async_trait]
pub trait BindEnv: Send + Sync {
    /// A digest of a provider's configuration now.
    fn provider_fingerprint(&self, provider: &ProviderName) -> Option<Digest>;
    /// A digest of an MCP server's tool list now.
    fn mcp_fingerprint(&self, server: &McpServerName) -> Option<Digest>;
    /// The tools an MCP server offers now, when the host can list them, so a
    /// changed tool list can be reported tool by tool. `None` (the default)
    /// reports the change for the server as a whole.
    fn mcp_tools(&self, _server: &McpServerName) -> Option<Vec<ToolDef>> {
        None
    }
    /// The providers configured on this machine now, for the `known` list
    /// of an issue about one that went away or changed. Empty (the default)
    /// when the host cannot say.
    fn providers_now(&self) -> Vec<String> {
        Vec::new()
    }
    /// The MCP servers configured or connected on this machine now, for the
    /// same. Empty (the default) when the host cannot say.
    fn mcp_servers_now(&self) -> Vec<String> {
        Vec::new()
    }
    /// Build the host's own live components for the run (compiled code, tool
    /// service state, connections).
    async fn bind(&self, spec: &RunSpec, code: &CodeFiles) -> Result<Bindings, SpawnIssues>;
}

type Insert = Box<dyn FnOnce(&mut EntityWorldMut<'_>) + Send>;
type AfterInsert = Box<dyn FnOnce(bevy_ecs::entity::Entity) + Send>;

/// The insertion step that runs `f` on an entity's `C`, if it has one.
fn editing<C: bevy_ecs::component::Component<Mutability = bevy_ecs::component::Mutable>>(
    f: Box<dyn FnOnce(&mut C) + Send>,
) -> Insert {
    Box::new(move |e: &mut EntityWorldMut<'_>| {
        if let Some(mut c) = e.get_mut::<C>() {
            f(&mut c);
        }
    })
}

/// Live components to place on a run's entity beside what its spec and state
/// provide, and what the host does once the entity exists (registering it
/// with a service keyed by entity). Built by binding; applied by insertion.
#[derive(Default)]
pub struct Bindings {
    inserts: Vec<Insert>,
    after: Vec<AfterInsert>,
}

impl Bindings {
    /// No live components.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a bundle of components.
    pub fn with<B: Bundle>(mut self, bundle: B) -> Self {
        self.inserts
            .push(Box::new(move |e: &mut EntityWorldMut<'_>| {
                e.insert(bundle);
            }));
        self
    }

    /// Change a component insertion placed, for a field only the host can
    /// fill (what the operator grants the run, say). Nothing happens when the
    /// entity has no such component.
    pub fn edit<C: bevy_ecs::component::Component<Mutability = bevy_ecs::component::Mutable>>(
        mut self,
        f: impl FnOnce(&mut C) + Send + 'static,
    ) -> Self {
        self.inserts.push(editing::<C>(Box::new(f)));
        self
    }

    /// Run `f` with the run's entity once it is placed.
    pub fn after_insert(
        mut self,
        f: impl FnOnce(bevy_ecs::entity::Entity) + Send + 'static,
    ) -> Self {
        self.after.push(Box::new(f));
        self
    }

    /// Add everything from another set of bindings.
    pub fn extend(&mut self, other: Bindings) {
        self.inserts.extend(other.inserts);
        self.after.extend(other.after);
    }

    /// How many bundles and follow-ups are waiting.
    pub fn len(&self) -> usize {
        self.inserts.len() + self.after.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn apply(self, entity: &mut EntityWorldMut<'_>) {
        let id = entity.id();
        for insert in self.inserts {
            insert(entity);
        }
        for after in self.after {
            after(id);
        }
    }
}

impl std::fmt::Debug for Bindings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Bindings({} bundles, {} follow-ups)",
            self.inserts.len(),
            self.after.len()
        )
    }
}
