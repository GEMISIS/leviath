//! [`DaemonEnv`]: what the daemon answers about its machine when a run is
//! resolved and bound.
//!
//! The runtime decides how a spawn is resolved ([`ResolveEnv`]) and what a
//! resolved run needs live ([`BindEnv`](leviath_runtime::spec::env::BindEnv)); this is the daemon's side of both.
//! It decides nothing new. Every answer is the daemon's existing machinery,
//! wrapped: installed blueprints are read the way `lev run` finds them, models
//! are chosen by the stage resolver over the provider registry and the
//! operator's `config.toml`, tools come from the built-ins, the sub-agent
//! tools, the connected MCP servers and the script tools, seeds run under the
//! same command and tool policies a spawn has always used, and a dependency is
//! judged by `crate::dependencies`.
//!
//! One env is built per spawn from the daemon's current state (the config as
//! it stands, the registry, the MCP tools the pool has connected), the same
//! way the spawn path builds its `SpawnDeps`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use leviath_providers::Tool;
use leviath_runtime::bind::host;
use leviath_runtime::host::SubAgentOp;
use leviath_runtime::interaction_hub::InteractionHub;
use leviath_runtime::pipeline::ToolOwners;
use leviath_runtime::spec::env::{
    CodeFiles, CodeUse, LoadedBlueprint, ModelPlan, OperatorDefaults, ResolveEnv, SeedCx,
    SpawnLimits, StageTools,
};
use leviath_runtime::spec::graph::{
    CodeRef, DependencyDef, MimeRows, Needs, RunGraph, Seed, StageDef,
};
use leviath_runtime::spec::inputs::PathKind;
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use leviath_runtime::spec::launch::Unattended;
use leviath_runtime::spec::names::{
    BlueprintPath, BlueprintRef, Digest, McpServerName, MimePattern, ModelRef, ProviderName, RunId,
    WorkdirPath,
};
use leviath_runtime::spec::run_spec::{AutoAnswers, SeededContent, ToolDef, ToolSource};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::Config;
use crate::daemon::tool_service::CliToolService;

mod bind;
mod layers;
mod seeds;

/// The longest part of a title a run id is minted from.
const RUN_ID_STEM_CHARS: usize = 48;

/// What the daemon knows about its machine for one spawn.
pub struct DaemonEnv {
    /// `config.toml` as it stands for this spawn.
    pub(crate) config: Arc<Config>,
    /// The providers runs infer on.
    pub(crate) registry: leviath_runtime::ProviderRegistry,
    /// Where installed blueprints live.
    pub(crate) agents_dir: Option<PathBuf>,
    /// The directory every workdir must be inside, when the operator set one.
    pub(crate) workdir_root: Option<PathBuf>,
    /// The tools the connected MCP servers advertise.
    pub(crate) mcp_defs: Vec<Tool>,
    /// Which server advertises each of them.
    pub(crate) mcp_owners: ToolOwners,
    /// The MCP connections every run shares.
    pub(crate) shared_mcp: Arc<tokio::sync::Mutex<leviath_mcp::ToolExecutor>>,
    /// The tool service a bound run's tool state is registered with.
    pub(crate) tool_service: Arc<CliToolService>,
    /// Where a run's prompts are parked.
    pub(crate) hub: InteractionHub,
    /// The channel a run's sub-agent tools send on.
    pub(crate) subagent_tx: UnboundedSender<SubAgentOp>,
    /// The mime registry attached bytes are typed by: the compiled rows with
    /// the operator's on top.
    pub(crate) mime: Arc<leviath_core::mime::MimeRegistry>,
    /// Where a run's stored parts go, which its tools and child runs share.
    pub(crate) blob_store: Arc<dyn leviath_core::mime::BlobStore>,
    /// The operator's reclassified MCP tools (`policy.toml`), which every
    /// taint gate applies.
    pub(crate) mcp_overrides: HashMap<String, leviath_core::policy::McpToolOverride>,
}

impl DaemonEnv {
    /// The MCP tools each connected server advertises, by server.
    fn mcp_by_server(&self) -> BTreeMap<McpServerName, Vec<Tool>> {
        let mut by_server: BTreeMap<McpServerName, Vec<Tool>> = BTreeMap::new();
        for tool in &self.mcp_defs {
            let owner = self
                .mcp_owners
                .get(&tool.name)
                .and_then(|o| McpServerName::new(o.as_str()).ok());
            if let Some(server) = owner {
                by_server.entry(server).or_default().push(tool.clone());
            }
        }
        by_server
    }

    /// An MCP server's tools now: `None` when the server is neither
    /// configured nor advertising anything.
    fn server_tools(&self, server: &McpServerName) -> Option<Vec<ToolDef>> {
        let tools = self.mcp_by_server().remove(server).unwrap_or_default();
        let configured = self
            .config
            .mcp_servers
            .iter()
            .any(|s| s.name == server.as_str());
        (configured || !tools.is_empty()).then(|| host::mcp_defs(server, &tools))
    }

    fn provider_print(&self, provider: &ProviderName) -> Option<Digest> {
        let creds = crate::commands::run::session::provider_creds_from_config(&self.config);
        match creds.iter().find(|c| c.name == provider.as_str()) {
            Some(creds) => Some(host::provider_fingerprint(creds)),
            None => self
                .registry
                .has(provider.as_str())
                .then(|| host::registered_fingerprint(provider.as_str())),
        }
    }

    fn mcp_print(&self, server: &McpServerName) -> Option<Digest> {
        self.server_tools(server)
            .map(|tools| host::tools_fingerprint(&tools))
    }

    /// The built-in tools (with the stage-control tools), the sub-agent tools
    /// and each connected MCP server's tools: every tool that is not a script.
    fn static_defs(&self) -> Vec<ToolDef> {
        let workdir = PathBuf::from(".");
        let builtins =
            leviath_tools::BuiltinTools::new(leviath_tools::ToolContext::new(workdir)).tool_defs();
        let mut defs = host::builtin_defs(&builtins);
        defs.extend(
            leviath_tools::BuiltinTools::subagent_tool_defs()
                .iter()
                .filter_map(|t| host::tool_def(t, ToolSource::Subagent)),
        );
        for (server, tools) in self.mcp_by_server() {
            defs.extend(host::mcp_defs(&server, &tools));
        }
        defs
    }

    /// Every tool a stage could be given: [`Self::static_defs`], then the
    /// script tools in the blueprint's own `tools/` (from `base`), then the
    /// global ones in `~/.leviath/tools`, then any the run's own code holds.
    /// The first of a name wins, and a script tool never shadows a tool that
    /// is not one.
    ///
    /// Each script tool found on disk comes back with its code, recorded
    /// under the path it was read from (blueprint-relative for the
    /// blueprint's own), so the run can carry it.
    fn catalog(&self, code: &CodeFiles, base: Option<&Path>) -> Catalog {
        let mut defs = self.static_defs();
        let mut taken: HashSet<String> = defs.iter().map(|d| d.name.to_string()).collect();
        let mut found = BTreeMap::new();
        let dirs: Vec<(PathBuf, Option<&Path>)> = base
            .map(|b| (b.join("tools"), Some(b)))
            .into_iter()
            .chain(leviath_core::tools_dir().map(|d| (d, None)))
            .collect();
        for (dir, under) in dirs {
            let (set, _, _) =
                crate::daemon::spawn::discover_script_tools_in(std::slice::from_ref(&dir), &taken);
            let on_disk = set
                .sources()
                .into_iter()
                .filter_map(|(meta, path)| std::fs::read(&path).ok().map(|b| (meta, path, b)));
            for (meta, path, bytes) in on_disk {
                if crate::daemon::spawn::current_platform_satisfies(&meta.required_caps)
                    && taken.insert(meta.name.clone())
                {
                    let shown = under
                        .and_then(|b| path.strip_prefix(b).ok())
                        .unwrap_or(&path);
                    let digest = Digest::of(&bytes);
                    defs.extend(script_def(&meta, digest.clone()));
                    let reference = CodeRef::File(shown.to_string_lossy().replace('\\', "/"));
                    found.insert(digest, (reference, bytes));
                }
            }
        }
        for (digest, bytes) in code {
            let meta = std::str::from_utf8(bytes)
                .ok()
                .and_then(|text| leviath_scripting::tool::check_source("code", text).ok());
            if let Some(meta) = meta.filter(|m| taken.insert(m.name.clone())) {
                defs.extend(script_def(&meta, digest.clone()));
            }
        }
        Catalog { defs, found }
    }
}

/// Every tool a stage could be given, and the code of the script tools that
/// were found on disk, by digest.
struct Catalog {
    defs: Vec<ToolDef>,
    found: BTreeMap<Digest, (CodeRef, Vec<u8>)>,
}

/// A script tool as the model is offered it.
fn script_def(meta: &leviath_scripting::ScriptToolMeta, digest: Digest) -> Option<ToolDef> {
    let tool = Tool {
        name: meta.name.clone(),
        description: meta.description.clone(),
        parameters: meta.parameters_schema(),
    };
    host::tool_def(&tool, ToolSource::Script(digest))
}

/// Load an installed blueprint as a run graph: `<agents_dir>/<name>/agent.toml`,
/// read by [`leviath_blueprint::find`]. The operator's defaults are not folded
/// in here; resolution does that for every graph, and checks it.
///
/// An installed copy of a bundled blueprint that will not read, and differs
/// from the one this build ships, is most likely out of date rather than
/// wrong, and the issue says so.
pub fn load_installed(
    agents_dir: Option<&Path>,
    reference: &BlueprintRef,
) -> Result<LoadedBlueprint, Box<SpawnIssue>> {
    let dirs: Vec<PathBuf> = agents_dir.map(Path::to_path_buf).into_iter().collect();
    leviath_blueprint::find(&dirs, reference).map_err(|mut issue| {
        if issue.code == IssueCode::Invalid
            && let Some(dir) = agents_dir
        {
            let file = dir
                .join(reference.name.as_str())
                .join(leviath_blueprint::FILE_NAME);
            issue
                .message
                .push_str(&crate::bundled::stale_install_suffix(
                    &file,
                    crate::bundled::real_agents_dir_opt().as_deref(),
                    ". ",
                ));
        }
        issue.hint = issue.hint.or_else(|| {
            (issue.code == IssueCode::Unresolvable && issue.path.0.is_empty())
                .then(|| "install it with `lev add`, or name one of these".to_string())
        });
        issue
    })
}

/// Load the blueprint in the directory `path` names, the way
/// [`load_installed`] loads an installed one. Named after its `[blueprint]
/// name`.
pub fn load_file(path: &BlueprintPath) -> Result<LoadedBlueprint, Box<SpawnIssue>> {
    // Relative to the source: the resolver puts `source.blueprint_file` in
    // front.
    let at = SpecPath::root();
    if !path.path().join(leviath_blueprint::FILE_NAME).is_file() {
        return Err(Box::new(
            SpawnIssue::new(
                at,
                IssueCode::Unresolvable,
                format!("no blueprint is in '{path}'"),
            )
            .hint("name the directory that holds the blueprint's agent.toml"),
        ));
    }
    leviath_blueprint::load(path.path()).map_err(|e| {
        Box::new(
            SpawnIssue::new(at, IssueCode::Invalid, e.to_string())
                .hint("run `lev validate` on it to see every problem"),
        )
    })
}

#[async_trait]
impl ResolveEnv for DaemonEnv {
    async fn blueprint(&self, reference: &BlueprintRef) -> Result<LoadedBlueprint, SpawnIssue> {
        load_installed(self.agents_dir.as_deref(), reference).map_err(|issue| *issue)
    }

    async fn blueprint_file(&self, path: &BlueprintPath) -> Result<LoadedBlueprint, SpawnIssue> {
        load_file(path).map_err(|issue| *issue)
    }

    fn limits(&self) -> SpawnLimits {
        let config = &self.config;
        SpawnLimits {
            default_max_depth: u8::try_from(crate::daemon::spawn::DEFAULT_SUBAGENT_DEPTH)
                .unwrap_or(u8::MAX),
            seed_commands_allowed: config.security.allow_seed_commands,
            max_attachment_bytes: config.max_part_bytes(),
            default_max_iterations: config
                .limits
                .default_max_iterations
                .map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
            defaults: OperatorDefaults {
                batch_tool_hint: config.batch_tool_hint,
                shell_hint: config.shell_hint,
                nudge: config.nudge.def(),
                taint_tracking: config.taint_tracking,
                capture_model_input: config.observability.capture_model_input,
            },
        }
    }

    fn compaction_model(&self, model: &ModelRef) -> Result<(), String> {
        host::compaction_model(
            model,
            &crate::daemon::spawn::model_defaults(&self.config),
            &self.registry,
        )
    }

    fn auto_answers(&self, unattended: &Unattended) -> Result<AutoAnswers, Box<SpawnIssue>> {
        let (profile, _) = layers::profile(unattended)
            .map_err(|e| {
                let known = match &e {
                    crate::yolo::YoloError::UnknownProfile { known, .. } => known.clone(),
                    _ => Vec::new(),
                };
                SpawnIssue::new(SpecPath::root(), IssueCode::Unresolvable, e.to_string())
                    .hint("name a profile `lev yolo` lists, or run unattended with `all`")
                    .known(known)
            })
            .map_err(Box::new)?;
        Ok(profile.map_or_else(AutoAnswers::default, |p| AutoAnswers {
            questions: p.spec.questions.is_auto(),
            checkpoints: p.spec.checkpoints.is_auto(),
            gate: p.spec.gate.is_auto(),
        }))
    }

    fn new_run_id(&self, title: &str) -> RunId {
        let stem: String = title.chars().take(RUN_ID_STEM_CHARS).collect();
        let stem = match stem.is_empty() {
            true => "run".to_string(),
            false => stem,
        };
        RunId::new(crate::runstate::new_run_id(&stem))
            .expect("a minted id uses only letters, digits and `-`")
    }

    fn workdir(&self, requested: Option<&Path>) -> Result<PathBuf, String> {
        let asked = requested.ok_or("a run started through the daemon names its workdir")?;
        let dir = std::fs::canonicalize(asked)
            .ok()
            .filter(|d| d.is_dir())
            .ok_or_else(|| {
                format!(
                    "workspace '{}' does not exist or is not a directory",
                    asked.display()
                )
            })?;
        match &self.workdir_root {
            Some(root) if !leviath_core::resolves_within(&dir, root) => Err(format!(
                "workdir '{}' is outside the configured --workdir-root '{}'",
                dir.display(),
                root.display()
            )),
            _ => Ok(dir),
        }
    }

    fn path_exists(&self, workdir: &Path, path: &WorkdirPath, kind: PathKind) -> bool {
        host::path_exists(workdir, path, kind)
    }

    async fn model(
        &self,
        stage: &StageDef,
        requested: Option<&ModelRef>,
    ) -> Result<ModelPlan, SpawnIssue> {
        host::choose_model(
            stage,
            requested,
            &crate::daemon::spawn::model_defaults(&self.config),
            &self.registry,
        )
        .map_err(|issue| *issue)
    }

    async fn tools(
        &self,
        _graph: &RunGraph,
        stage: &StageDef,
        code: &CodeFiles,
        base: Option<&Path>,
    ) -> Result<StageTools, SpawnIssues> {
        let mut catalog = self.catalog(code, base);
        let tools = host::select_tools(&catalog.defs, stage)?;
        let code = tools
            .iter()
            .filter_map(|t| match &t.source {
                ToolSource::Script(digest) => catalog.found.remove(digest),
                _ => None,
            })
            .collect();
        Ok(StageTools { tools, code })
    }

    async fn code(&self, code: &CodeRef, base: Option<&Path>) -> Result<Vec<u8>, String> {
        host::read_code(code, base)
    }

    fn check_code(&self, code: &[u8], used_as: CodeUse) -> Result<(), String> {
        host::check_code(code, used_as)?;
        match used_as {
            CodeUse::Seed => rhai::Engine::new()
                .compile(String::from_utf8_lossy(code).as_ref())
                .map(drop)
                .map_err(|e| e.to_string()),
            _ => Ok(()),
        }
    }

    async fn seed(&self, seed: &Seed, cx: SeedCx<'_>) -> Result<SeededContent, String> {
        seeds::run(self, seed, cx).map(|text| SeededContent {
            text,
            parts: Vec::new(),
        })
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
        if let Needs::Check(_) = &dependency.needs {
            return host::run_check(code);
        }
        // Asked only about what must be in place, so judged as required.
        let required = DependencyDef {
            required: true,
            ..dependency.clone()
        };
        let report = crate::dependencies::evaluate(
            std::slice::from_ref(&required),
            &self.config.mcp_servers,
            Path::new("."),
            &crate::dependencies::SystemProbe,
        );
        match report.blocking().first() {
            None => Ok(()),
            Some(status) => Err(status.state.detail().to_string()),
        }
    }

    fn provider_fingerprint(&self, provider: &ProviderName) -> Option<Digest> {
        self.provider_print(provider)
    }

    fn mcp_fingerprint(&self, server: &McpServerName) -> Option<Digest> {
        self.mcp_print(server)
    }
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
mod parity_tests;
