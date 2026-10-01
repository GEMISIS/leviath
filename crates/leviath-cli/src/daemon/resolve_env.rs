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

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use leviath_providers::Tool;
use leviath_runtime::bind::host;
use leviath_runtime::host::SubAgentOp;
use leviath_runtime::interaction_hub::InteractionHub;
use leviath_runtime::pipeline::ToolOwners;
use leviath_runtime::spec::env::{
    CodeFiles, CodeUse, LoadedBlueprint, ModelPlan, ResolveEnv, SeedCx, SpawnLimits,
};
use leviath_runtime::spec::graph::{CodeRef, DependencyDef, Needs, RunGraph, Seed, StageDef};
use leviath_runtime::spec::inputs::PathKind;
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use leviath_runtime::spec::names::{
    BlueprintRef, Digest, McpServerName, MimePattern, ModelRef, ProviderName, RunId, WorkdirPath,
};
use leviath_runtime::spec::run_spec::{SeededContent, ToolDef, ToolSource};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::Config;
use crate::daemon::tool_service::CliToolService;

mod bind;
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
    /// The mime registry attached bytes are typed by.
    pub(crate) mime: Arc<leviath_core::mime::MimeRegistry>,
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

    /// Every tool a stage could be given: the built-ins (with the
    /// stage-control tools), the sub-agent tools, each connected MCP server's
    /// tools, the global script tools, and any script tool the run's own code
    /// holds. A script tool whose name another tool already has is left out,
    /// so it never shadows one.
    fn catalog(&self, code: &CodeFiles) -> Vec<ToolDef> {
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
        let mut taken: HashSet<String> = defs.iter().map(|d| d.name.to_string()).collect();
        let dirs: Vec<PathBuf> = leviath_core::tools_dir().into_iter().collect();
        let (set, _, _) = crate::daemon::spawn::discover_script_tools_in(&dirs, &taken);
        let on_disk = set
            .sources()
            .into_iter()
            .filter_map(|(meta, path)| std::fs::read(path).ok().map(|bytes| (meta, bytes)));
        for (meta, bytes) in on_disk {
            if crate::daemon::spawn::current_platform_satisfies(&meta.required_caps)
                && taken.insert(meta.name.clone())
            {
                defs.extend(script_def(&meta, Digest::of(&bytes)));
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
        defs
    }
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

/// The installed blueprints' names, for a refusal's `known` list.
fn installed(agents_dir: Option<&Path>) -> Vec<String> {
    let mut names: Vec<String> = agents_dir
        .and_then(|d| std::fs::read_dir(d).ok())
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .join(leviath_core::files::MANIFEST_FILENAME)
                .is_file()
        })
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Load an installed blueprint as a run graph: `<agents_dir>/<name>/agent.leviath`,
/// parsed and validated as every spawn has done, with the operator's
/// `default_max_iterations` filled into each stage that sets no ceiling of its
/// own (a stage writing `0` asked for none, and does not get to opt out of the
/// operator's).
///
/// The one place the blueprint format is read, so the file format can change
/// behind it.
pub fn load_installed(
    agents_dir: Option<&Path>,
    reference: &BlueprintRef,
    config: &Config,
) -> Result<LoadedBlueprint, Box<SpawnIssue>> {
    let at = SpecPath::root().field("source").field("blueprint");
    let name = &reference.name;
    let issue = |code: IssueCode, message: String| SpawnIssue::new(at.clone(), code, message);
    let (manifest, content) = agents_dir
        .map(|d| {
            d.join(name.as_str())
                .join(leviath_core::files::MANIFEST_FILENAME)
        })
        .and_then(|p| std::fs::read_to_string(&p).ok().map(|c| (p, c)))
        .ok_or_else(|| {
            issue(
                IssueCode::Unresolvable,
                format!("no blueprint named '{name}' is installed"),
            )
            .hint("install it with `lev add`, or name one of these")
            .known(installed(agents_dir))
        })?;
    let digest = Digest::of(content.as_bytes());
    if let Some(pinned) = reference.digest.as_ref().filter(|p| **p != digest) {
        return Err(issue(
            IssueCode::Unresolvable,
            format!("blueprint '{name}' is installed at a different revision"),
        )
        .expected(format!("revision {pinned}"))
        .got(format!("revision {digest}"))
        .into());
    }
    let stale = crate::bundled::stale_install_suffix(
        &manifest,
        crate::bundled::real_agents_dir_opt().as_deref(),
        ". ",
    );
    let blueprint = leviath_runtime::spec::manifest::parse_manifest(&content)
        .map_err(|e| issue(IssueCode::Invalid, format!("parse manifest: {e}{stale}")))?;
    blueprint
        .validate()
        .map_err(|e| issue(IssueCode::Invalid, format!("invalid blueprint: {e}{stale}")))?;
    let mut graph = RunGraph::from_blueprint(&blueprint).map_err(|issues| {
        let each: Vec<String> = issues.iter().map(ToString::to_string).collect();
        issue(
            IssueCode::Invalid,
            format!(
                "blueprint '{name}' does not read as a run graph: {}",
                each.join("; ")
            ),
        )
    })?;
    if let Some(ceiling) = config.limits.default_max_iterations {
        let ceiling = u32::try_from(ceiling).unwrap_or(u32::MAX);
        for stage in &mut graph.stages {
            if matches!(stage.max_iterations, None | Some(0)) {
                stage.max_iterations = Some(ceiling);
            }
        }
    }
    Ok(LoadedBlueprint {
        graph,
        reference: BlueprintRef {
            name: name.clone(),
            digest: Some(digest),
        },
        version: blueprint.version,
        base_dir: manifest.parent().map(Path::to_path_buf).unwrap_or_default(),
    })
}

#[async_trait]
impl ResolveEnv for DaemonEnv {
    async fn blueprint(&self, reference: &BlueprintRef) -> Result<LoadedBlueprint, SpawnIssue> {
        load_installed(self.agents_dir.as_deref(), reference, &self.config).map_err(|issue| *issue)
    }

    fn limits(&self) -> SpawnLimits {
        SpawnLimits {
            default_max_depth: u8::try_from(crate::daemon::spawn::DEFAULT_SUBAGENT_DEPTH)
                .unwrap_or(u8::MAX),
            seed_commands_allowed: self.config.security.allow_seed_commands,
            max_attachment_bytes: self.config.max_part_bytes(),
        }
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
    ) -> Result<Vec<ToolDef>, SpawnIssues> {
        host::select_tools(&self.catalog(code), stage)
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

    fn sniff(
        &self,
        name: &str,
        bytes: &[u8],
        declared: Option<&MimePattern>,
    ) -> Result<String, String> {
        host::sniff(&self.mime, name, bytes, declared)
    }

    async fn dependency(&self, dependency: &DependencyDef) -> Result<(), String> {
        use leviath_runtime::spec::blueprint::{Dependency, DependencyKind};
        let kind = match &dependency.needs {
            Needs::McpServer { server, env } => DependencyKind::McpServer {
                server: server.to_string(),
                env: env.clone(),
            },
            Needs::Env(var) => DependencyKind::Env { var: var.clone() },
            Needs::Binary(command) => DependencyKind::Binary {
                command: command.clone(),
            },
            Needs::Check(CodeRef::Inline(source)) => return host::run_inline_check(source),
            Needs::Check(CodeRef::File(file)) => {
                return Err(format!(
                    "the check '{file}' is named by file, and a dependency is judged from the \
                     run's own code; write the check inline"
                ));
            }
        };
        let legacy = Dependency {
            name: dependency.name.clone(),
            kind,
            required: true,
            remedy: dependency.remedy.clone(),
            description: None,
            install: None,
        };
        let report = crate::dependencies::evaluate(
            &[legacy],
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
