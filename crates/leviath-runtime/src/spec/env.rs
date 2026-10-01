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

use super::graph::{CodeRef, DependencyDef, RunGraph, Seed, StageDef};
use super::inputs::PathKind;
use super::issues::{SpawnIssue, SpawnIssues};
use super::launch::LaunchPolicy;
use super::names::{
    BlueprintRef, Digest, McpServerName, MimePattern, ModelId, ModelRef, ProviderName, RunId,
    StageName, WorkdirPath,
};
use super::run_spec::{RunSpec, SeededContent, ToolDef};

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
    },
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
}

/// The model a stage runs on, chosen.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelPlan {
    /// The provider.
    pub provider: ProviderName,
    /// The model.
    pub model: ModelId,
    /// Its context window, in tokens.
    pub context_window: u32,
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
    /// A script tool.
    Tool,
}

/// What a seed can see while it runs.
#[derive(Debug, Clone, Copy)]
pub struct SeedCx<'a> {
    /// The run's workdir.
    pub workdir: &'a Path,
    /// Whether shell-command seeds may run.
    pub commands_allowed: bool,
    /// The code the graph names, by digest, already read.
    pub code: &'a CodeFiles,
    /// The run's checked inputs.
    pub inputs: &'a super::inputs::InputValues,
}

/// Code read for a run, by digest.
pub type CodeFiles = BTreeMap<Digest, Vec<u8>>;

/// Every question resolving a run asks of the machine.
#[async_trait]
pub trait ResolveEnv: Send + Sync {
    /// Load an installed blueprint.
    async fn blueprint(&self, reference: &BlueprintRef) -> Result<LoadedBlueprint, SpawnIssue>;
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
    /// model, for a stage that allows one.
    async fn model(
        &self,
        stage: &StageDef,
        requested: Option<&ModelRef>,
    ) -> Result<ModelPlan, SpawnIssue>;
    /// The tools a stage gets, each with its schema. `code` holds the run's
    /// code already read, for script tools.
    async fn tools(
        &self,
        graph: &RunGraph,
        stage: &StageDef,
        code: &CodeFiles,
    ) -> Result<Vec<ToolDef>, SpawnIssues>;
    /// Read code the graph names. `base` is the blueprint's directory, when
    /// the graph came from one.
    async fn code(&self, code: &CodeRef, base: Option<&Path>) -> Result<Vec<u8>, String>;
    /// Check that code compiles and has what `used_as` calls.
    fn check_code(&self, code: &[u8], used_as: CodeUse) -> Result<(), String>;
    /// Run a spawn-time seed.
    async fn seed(&self, seed: &Seed, cx: SeedCx<'_>) -> Result<SeededContent, String>;
    /// The mime type of attached bytes: the declared one if they match it,
    /// else sniffed from the bytes and the name.
    fn sniff(
        &self,
        name: &str,
        bytes: &[u8],
        declared: Option<&MimePattern>,
    ) -> Result<String, String>;
    /// Whether a dependency is met.
    async fn dependency(&self, dependency: &DependencyDef) -> Result<(), String>;
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
    /// Build the host's own live components for the run (compiled code, tool
    /// service state, connections).
    async fn bind(&self, spec: &RunSpec, code: &CodeFiles) -> Result<Bindings, SpawnIssues>;
}

type Insert = Box<dyn FnOnce(&mut EntityWorldMut<'_>) + Send>;
type AfterInsert = Box<dyn FnOnce(bevy_ecs::entity::Entity) + Send>;

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
