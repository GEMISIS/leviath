//! Assembling one agent's tool state: what it may call, and under what policy.
//!
//! The layering all lands here - built-ins, MCP tools, script tools, the stage
//! and agent permission tables, launch overrides, the sandbox - which is why the
//! inputs travel as [`ToolStateParts`] rather than as a parameter list.

use super::*;

/// Build one agent's [`AgentToolState`] from the shared executors + config.
///
/// `stage_perms_by_index` holds every stage's `[tool_permissions]` (in stage
/// order); the entry stage's map seeds `stage_perms`, and the pipeline's
/// `sync_stage` swaps in the right one as the agent changes stage.
/// Everything [`build_tool_state`] assembles an agent's tool state from.
///
/// A struct rather than twenty-one positional parameters, and the reason is
/// narrower than the lint: three of them - `run_id`, `entry_stage` and
/// `agent_name` - are all `&str`. Transposing any two of those compiles
/// silently and produces a run whose approvals are keyed to the wrong name.
/// Nothing else in this file would catch that.
pub(super) struct ToolStateParts<'a> {
    /// The run's write budget, already spent on by the seeds.
    pub(super) writes: Arc<crate::daemon::tool_service::WriteBudget>,
    /// The built-in tools, over this agent's workdir.
    pub(super) builtins: Arc<leviath_tools::BuiltinTools>,
    /// Their names, for deciding what is a builtin at dispatch.
    pub(super) builtin_names: HashSet<String>,
    /// MCP connections shared across agents.
    pub(super) mcp: Arc<Mutex<leviath_mcp::ToolExecutor>>,
    /// The resolved daemon configuration.
    pub(super) config: &'a Config,
    /// Where this agent's prompts are parked.
    pub(super) hub: &'a InteractionHub,
    /// This run's id, which grants are recorded against.
    pub(super) run_id: &'a str,
    /// The stage the agent enters at.
    pub(super) entry_stage: &'a str,
    /// That stage's index in the blueprint.
    pub(super) entry_index: usize,
    /// Per-stage tool policies, indexed by stage.
    pub(super) stage_perms_by_index: Vec<HashMap<String, String>>,
    /// Per-stage required tools, indexed by stage.
    pub(super) stage_required_by_index: Vec<HashSet<String>>,
    /// Agent-wide tool policies from the blueprint.
    pub(super) agent_perms: HashMap<String, String>,
    /// The blueprint's name, for policy lookup and messages.
    pub(super) agent_name: &'a str,
    /// `--allow` / `--yolo` overrides for this launch.
    pub(super) launch_overrides: HashMap<String, crate::config::ToolPolicy>,
    /// Handle for the sub-agent tools, when this agent may spawn.
    pub(super) subagent: Option<SubAgentHandle>,
    /// The sandbox shell calls run in, when one is configured.
    pub(super) sandbox: Option<Arc<crate::daemon::sandbox_manager::SandboxManager>>,
    /// Rhai tools discovered for this agent.
    pub(super) script_tools: leviath_scripting::ScriptToolSet,
    /// Their names, kept apart so a rescan can diff against them.
    pub(super) script_tool_names: HashSet<String>,
    /// The host those scripts call back into.
    pub(super) script_host: Arc<dyn leviath_scripting::ScriptHost>,
    /// Re-resolution context, for a blueprint that rescans mid-run.
    pub(super) dynamic: Option<Arc<crate::daemon::tool_service::DynamicToolCtx>>,
    /// Whether this run answers its own prompts (`--yolo`).
    pub(super) unattended: bool,
    /// `[safe_commands]` the blueprint declares, if the user opted in.
    pub(super) blueprint_safe: Option<&'a leviath_core::blueprint::SafeCommandsConfig>,
    /// `[read_paths]` the blueprint declares, if any.
    pub(super) blueprint_read_paths: Option<&'a leviath_core::blueprint::ReadPathsConfig>,
    /// The run's workdir, which read-path entries compile relative to.
    pub(super) workdir: std::path::PathBuf,
}

pub(super) fn build_tool_state(parts: ToolStateParts<'_>) -> Arc<AgentToolState> {
    let entry_perms = parts
        .stage_perms_by_index
        .get(parts.entry_index)
        .cloned()
        .unwrap_or_default();
    let entry_required = parts
        .stage_required_by_index
        .get(parts.entry_index)
        .cloned()
        .unwrap_or_default();
    Arc::new(AgentToolState {
        // One budget per run, so the per-run ceiling spans every batch rather
        // than resetting with each one - and spans the seeds before them.
        writes: parts.writes,
        builtins: parts.builtins,
        mcp: parts.mcp,
        builtin_names: parts.builtin_names,
        launch_overrides: Arc::new(parts.launch_overrides),
        safe_keys: crate::daemon::tool_service::Live::new(
            parts
                .config
                .safe_keys_for_agent(parts.agent_name, parts.blueprint_safe)
                .into_keys()
                .collect(),
        ),
        run_allows: Arc::new(Mutex::new(HashSet::new())),
        stage_allows: Arc::new(StdMutex::new(HashSet::new())),
        stage_allows_index: Arc::new(StdMutex::new(None)),
        stage_perms: Arc::new(StdMutex::new(entry_perms)),
        stage_perms_by_index: Arc::new(parts.stage_perms_by_index),
        stage_required: Arc::new(StdMutex::new(entry_required)),
        stage_required_by_index: Arc::new(parts.stage_required_by_index),
        agent_perms: Arc::new(parts.agent_perms),
        blueprint_may_loosen: Arc::new(std::sync::atomic::AtomicBool::new(
            parts.config.security.allow_blueprint_permissions,
        )),
        // The ceiling a blueprint may tighten but not loosen: the user's global
        // `[tool_permissions]` plus any `[agent_tool_permissions.<name>]` grant
        // they made for this specific agent. Flattened here so every later
        // `resolve_policy` reads one map, and re-flattened on resume.
        global_perms: crate::daemon::tool_service::Live::new(
            parts.config.permissions_for_agent(parts.agent_name),
        ),
        interaction: parts.hub.backend_for(parts.run_id),
        unattended: parts.unattended,
        stage_name: Arc::new(StdMutex::new(parts.entry_stage.to_string())),
        subagent: parts.subagent,
        sandbox: parts.sandbox,
        script_tools: Arc::new(StdMutex::new(parts.script_tools)),
        script_tool_names: Arc::new(StdMutex::new(parts.script_tool_names)),
        script_host: parts.script_host,
        dynamic: parts.dynamic,
        // Everything a resume needs to redo this resolution against the config
        // as it stands then, rather than the copy this spawn read.
        config_source: Arc::new(crate::daemon::tool_service::ConfigSource {
            agent_name: parts.agent_name.to_string(),
            blueprint_safe: parts.blueprint_safe.cloned(),
            blueprint_read_paths: parts.blueprint_read_paths.cloned(),
            workdir: parts.workdir,
        }),
    })
}
