//! [`EmbedEnv`]: what an embedded world knows about its machine, for resolving
//! and binding runs.
//!
//! An embedder has a provider registry, the model defaults it set on the
//! builder, the blueprints it chose to register, and the default tool service.
//! It has no daemon config, no MCP servers and no sandboxes, so the answers
//! here are the plain ones: shell-command, tool and code seeds are refused,
//! and a dependency on an MCP server is never met.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use super::BasicToolService;
use crate::bind::host;
use crate::pipeline::ModelDefaults;
use crate::provider_creds::ProviderCreds;
use crate::providers::ProviderRegistry;
use crate::spec::env::{
    BindEnv, Bindings, CodeFiles, CodeUse, LoadedBlueprint, ModelPlan, ResolveEnv, SeedCx,
    SpawnLimits, StageTools,
};
use crate::spec::graph::{CodeRef, DependencyDef, MimeRows, Needs, RunGraph, Seed, StageDef};
use crate::spec::inputs::PathKind;
use crate::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use crate::spec::names::{
    BlueprintRef, Digest, McpServerName, MimePattern, ModelRef, ProviderName, RunId, WorkdirPath,
};
use crate::spec::run_spec::{RunSpec, SeededContent};

/// The child-run depth an embedded run gets when it does not say.
const DEFAULT_MAX_DEPTH: u8 = 3;

/// The longest part of a title a run id is minted from.
const RUN_ID_STEM_CHARS: usize = 48;

/// What an embedded world can answer about its machine.
pub struct EmbedEnv {
    registry: ProviderRegistry,
    defaults: ModelDefaults,
    creds: Vec<ProviderCreds>,
    blueprints: BTreeMap<String, LoadedBlueprint>,
    limits: SpawnLimits,
    workdir: Option<PathBuf>,
    mime: leviath_core::mime::MimeRegistry,
    tools: Option<Arc<BasicToolService>>,
}

impl EmbedEnv {
    /// An env over `registry`, choosing models by `defaults`.
    pub fn new(registry: ProviderRegistry, defaults: ModelDefaults) -> Self {
        Self {
            registry,
            defaults,
            creds: Vec::new(),
            blueprints: BTreeMap::new(),
            limits: SpawnLimits {
                default_max_depth: DEFAULT_MAX_DEPTH,
                seed_commands_allowed: false,
                max_attachment_bytes: crate::blob_store::MimeLimits::default().max_part_bytes,
                default_max_iterations: None,
                defaults: Default::default(),
            },
            workdir: None,
            mime: leviath_core::mime::MimeRegistry::builtin(),
            tools: None,
        }
    }

    /// The credentials the registry was built from, which a provider's
    /// fingerprint is taken over. A provider registered without any is
    /// fingerprinted by its name alone.
    pub fn with_creds(mut self, creds: Vec<ProviderCreds>) -> Self {
        self.creds = creds;
        self
    }

    /// A blueprint runs may name, under its reference's name.
    pub fn with_blueprint(mut self, blueprint: LoadedBlueprint) -> Self {
        self.blueprints
            .insert(blueprint.reference.name.to_string(), blueprint);
        self
    }

    /// The limits and defaults spawns answer to.
    pub fn with_limits(mut self, limits: SpawnLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The workdir a run that names none works in.
    pub fn with_default_workdir(mut self, workdir: impl Into<PathBuf>) -> Self {
        self.workdir = Some(workdir.into());
        self
    }

    /// The mime registry attached bytes are typed by.
    pub fn with_mime_registry(mut self, mime: leviath_core::mime::MimeRegistry) -> Self {
        self.mime = mime;
        self
    }

    /// The default tool service, which each bound run is registered with.
    pub fn with_basic_tools(mut self, tools: Arc<BasicToolService>) -> Self {
        self.tools = Some(tools);
        self
    }

    fn fingerprint(&self, provider: &ProviderName) -> Option<Digest> {
        match self.creds.iter().find(|c| c.name == provider.as_str()) {
            Some(creds) => Some(host::provider_fingerprint(creds)),
            None => self
                .registry
                .has(provider.as_str())
                .then(|| host::registered_fingerprint(provider.as_str())),
        }
    }
}

/// Whether a program is a file in some directory of `path`.
fn on_path(path: Option<std::ffi::OsString>, program: &str) -> bool {
    path.is_some_and(|path| {
        std::env::split_paths(&path)
            .any(|dir| dir.join(program).is_file() || dir.join(format!("{program}.exe")).is_file())
    })
}

#[async_trait]
impl ResolveEnv for EmbedEnv {
    async fn blueprint(&self, reference: &BlueprintRef) -> Result<LoadedBlueprint, SpawnIssue> {
        // Relative to the reference: resolving places it under
        // `source.blueprint`.
        let path = SpecPath::root();
        let loaded = self
            .blueprints
            .get(reference.name.as_str())
            .ok_or_else(|| {
                SpawnIssue::new(
                    path.clone(),
                    IssueCode::Unresolvable,
                    format!("no blueprint named '{}' is registered", reference.name),
                )
                .known(self.blueprints.keys())
            })?;
        match &reference.digest {
            Some(digest) if loaded.reference.digest.as_ref() != Some(digest) => {
                Err(SpawnIssue::new(
                    path,
                    IssueCode::Unresolvable,
                    format!(
                        "blueprint '{}' is registered at a different revision",
                        reference.name
                    ),
                )
                .expected(format!("revision {digest}"))
                .got(match &loaded.reference.digest {
                    Some(d) => format!("revision {d}"),
                    None => "no recorded revision".to_string(),
                }))
            }
            _ => Ok(loaded.clone()),
        }
    }

    fn limits(&self) -> SpawnLimits {
        self.limits.clone()
    }

    fn new_run_id(&self, title: &str) -> RunId {
        let stem: String = title
            .chars()
            .take(RUN_ID_STEM_CHARS)
            .map(|c| match c.is_ascii_alphanumeric() {
                true => c.to_ascii_lowercase(),
                false => '-',
            })
            .collect();
        let stem = match stem.is_empty() {
            true => "agent".to_string(),
            false => stem,
        };
        RunId::new(crate::spec::names::mint_run_id(&stem))
            .expect("a minted id uses only letters, digits and `-`")
    }

    fn workdir(&self, requested: Option<&Path>) -> Result<PathBuf, String> {
        let dir = requested
            .map(Path::to_path_buf)
            .or_else(|| self.workdir.clone())
            .ok_or("an embedded run names its workdir; none was given and no default is set")?;
        std::fs::canonicalize(&dir)
            .ok()
            .filter(|d| d.is_dir())
            .ok_or_else(|| {
                format!(
                    "workspace '{}' does not exist or is not a directory",
                    dir.display()
                )
            })
    }

    fn path_exists(&self, workdir: &Path, path: &WorkdirPath, kind: PathKind) -> bool {
        host::path_exists(workdir, path, kind)
    }

    async fn model(
        &self,
        stage: &StageDef,
        requested: Option<&ModelRef>,
    ) -> Result<ModelPlan, SpawnIssue> {
        host::choose_model(stage, requested, &self.defaults, &self.registry).map_err(|issue| *issue)
    }

    fn compaction_model(&self, model: &ModelRef) -> Result<(), String> {
        host::compaction_model(model, &self.defaults, &self.registry)
    }

    async fn tools(
        &self,
        _graph: &RunGraph,
        stage: &StageDef,
        _code: &CodeFiles,
        _base: Option<&Path>,
        _workdir: Option<&Path>,
    ) -> Result<StageTools, SpawnIssues> {
        let catalog = host::builtin_defs(&BasicToolService::tool_defs(Path::new(".")));
        host::select_tools(&catalog, stage).map(StageTools::from)
    }

    async fn code(&self, code: &CodeRef, base: Option<&Path>) -> Result<Vec<u8>, String> {
        host::read_code(code, base)
    }

    fn check_code(&self, code: &[u8], used_as: CodeUse) -> Result<(), String> {
        host::check_code(code, used_as)
    }

    async fn seed(&self, seed: &Seed, _cx: SeedCx<'_>) -> Result<SeededContent, String> {
        let kind = match seed {
            Seed::Literal(text) => {
                return Ok(SeededContent {
                    text: text.clone(),
                    parts: Vec::new(),
                });
            }
            Seed::Glob(_) => "glob",
            Seed::Files(_) => "files",
            Seed::Code(_) => "code",
            Seed::Command(_) => "command",
            Seed::Tools { .. } => "tool",
        };
        Err(format!(
            "an embedded world does not run {kind} seeds; pass the content as an input instead"
        ))
    }

    fn mime_registry(&self, rows: &MimeRows) -> Result<leviath_core::mime::MimeRegistry, String> {
        host::run_registry(&self.mime, rows)
    }

    fn sniff(
        &self,
        registry: &leviath_core::mime::MimeRegistry,
        name: &str,
        bytes: &[u8],
        declared: Option<&MimePattern>,
    ) -> Result<String, String> {
        host::sniff(registry, name, bytes, declared)
    }

    async fn dependency(
        &self,
        dependency: &DependencyDef,
        code: Option<&[u8]>,
    ) -> Result<(), String> {
        let unmet = |default: String| Err(dependency.remedy.clone().unwrap_or(default));
        match &dependency.needs {
            Needs::Env(var) => match std::env::var(var).is_ok_and(|v| !v.trim().is_empty()) {
                true => Ok(()),
                false => unmet(format!("set the environment variable {var}")),
            },
            Needs::Binary(program) => match on_path(std::env::var_os("PATH"), program) {
                true => Ok(()),
                false => unmet(format!("install '{program}' and put it on your PATH")),
            },
            Needs::McpServer { server, .. } => unmet(format!(
                "'{server}' is an MCP server, and an embedded world connects none"
            )),
            Needs::Check(_) => host::run_check(code),
        }
    }

    fn provider_fingerprint(&self, provider: &ProviderName) -> Option<Digest> {
        self.fingerprint(provider)
    }

    fn mcp_fingerprint(&self, _server: &McpServerName) -> Option<Digest> {
        None
    }
}

#[async_trait]
impl BindEnv for EmbedEnv {
    fn provider_fingerprint(&self, provider: &ProviderName) -> Option<Digest> {
        self.fingerprint(provider)
    }

    fn mcp_fingerprint(&self, _server: &McpServerName) -> Option<Digest> {
        None
    }

    async fn bind(&self, spec: &RunSpec, code: &CodeFiles) -> Result<Bindings, SpawnIssues> {
        let registry = crate::bind::scripts::mime_registry(spec, code, &self.mime)?;
        let mut bindings = crate::bind::scripts::compile(spec, code)?.with(registry);
        if let Some(tools) = self.tools.clone() {
            let run_id = spec.run_id.to_string();
            let workdir = spec.placement.workdir.clone();
            bindings =
                bindings.after_insert(move |entity| tools.register(entity, &run_id, workdir));
        }
        Ok(bindings)
    }
}

#[cfg(test)]
#[path = "env_tests.rs"]
mod tests;
