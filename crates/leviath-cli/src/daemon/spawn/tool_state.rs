//! Assembling one agent's tool state: what it may call, and under what policy.
//!
//! The layering all lands here - built-ins, MCP tools, script tools, the stage
//! and agent permission tables, launch overrides, the sandbox - which is why the
//! inputs travel as [`ToolStateParts`] rather than as a parameter list.

use super::*;

/// Everything [`build_tool_state`] assembles an agent's tool state from.
///
/// A struct rather than twenty-one positional parameters, and the reason is
/// narrower than the lint: three of them - `run_id`, `entry_stage` and
/// `agent_name` - are all `&str`. Transposing any two of those compiles
/// silently and produces a run whose approvals are keyed to the wrong name.
/// Nothing else in this file would catch that.
pub(crate) struct ToolStateParts<'a> {
    /// The run's write budget, already spent on by the seeds.
    pub(crate) writes: Arc<crate::daemon::tool_service::WriteBudget>,
    /// The built-in tools, over this agent's workdir.
    pub(crate) builtins: Arc<leviath_tools::BuiltinTools>,
    /// Their names, for deciding what is a builtin at dispatch.
    pub(crate) builtin_names: HashSet<String>,
    /// MCP connections shared across agents.
    pub(crate) mcp: Arc<Mutex<leviath_mcp::ToolExecutor>>,
    /// The resolved daemon configuration.
    pub(crate) config: &'a Config,
    /// Where this agent's prompts are parked.
    pub(crate) hub: &'a InteractionHub,
    /// This run's id, which grants are recorded against.
    pub(crate) run_id: &'a str,
    /// The stage the agent enters at.
    pub(crate) entry_stage: &'a str,
    /// That stage's index in the blueprint.
    pub(crate) entry_index: usize,
    /// Per-stage tool policies, indexed by stage.
    pub(crate) stage_perms_by_index: Vec<HashMap<String, String>>,
    /// Per-stage required tools, indexed by stage.
    pub(crate) stage_required_by_index: Vec<HashSet<String>>,
    /// Per-stage `tool_accepts`, by canonical tool name, indexed by stage.
    pub(crate) stage_tool_accepts_by_index: Vec<HashMap<String, Vec<String>>>,
    /// Agent-wide tool policies from the blueprint.
    pub(crate) agent_perms: HashMap<String, String>,
    /// The blueprint's name, for policy lookup and messages.
    pub(crate) agent_name: &'a str,
    /// `--allow` / `--yolo` overrides for this launch.
    pub(crate) launch_overrides: HashMap<String, crate::config::ToolPolicy>,
    /// Handle for the sub-agent tools, when this agent may spawn.
    pub(crate) subagent: Option<SubAgentHandle>,
    /// The sandbox shell calls run in, when one is configured.
    pub(crate) sandbox: Option<Arc<crate::daemon::sandbox_manager::SandboxManager>>,
    /// Rhai tools discovered for this agent.
    pub(crate) script_tools: leviath_scripting::ScriptToolSet,
    /// Their names, kept apart so a rescan can diff against them.
    pub(crate) script_tool_names: HashSet<String>,
    /// The host those scripts call back into.
    pub(crate) script_host: Arc<dyn leviath_scripting::ScriptHost>,
    /// The parts handle that host reads, which the runtime's offers fill.
    pub(crate) offered_parts: Arc<std::sync::Mutex<Vec<leviath_core::mime::Part>>>,
    /// Re-resolution context, for a blueprint that rescans mid-run.
    pub(crate) dynamic: Option<Arc<crate::daemon::tool_service::DynamicToolCtx>>,
    /// Whether this run answers its own prompts: `--yolo` under a profile
    /// whose `questions` are `auto`.
    pub(crate) unattended: bool,
    /// The yolo profile this run decides tool calls under, if it is a yolo
    /// run at all.
    pub(crate) yolo: Option<Arc<crate::yolo::YoloProfile>>,
    /// The profile's name when `--yolo=<name>` named one, so a resume can read
    /// it again. `None` for an attended run and for the bare flag.
    pub(crate) yolo_profile: Option<String>,
    /// The files this run may not change, shared with the seeds that ran
    /// before the tool lane existed.
    pub(crate) protected: Vec<crate::tools::ProtectedPath>,
    /// The graph's `safe_commands`, when it declares any; they count only if the user opted in.
    pub(crate) blueprint_safe: Option<&'a leviath_runtime::spec::graph::SafeCommandsDef>,
    /// The graph's `read_paths`, empty when it declares none.
    pub(crate) blueprint_read_paths: &'a [String],
    /// The run's workdir, which read-path entries compile relative to.
    pub(crate) workdir: std::path::PathBuf,
}

/// Build one agent's [`AgentToolState`] from the shared executors + config.
///
/// `stage_perms_by_index` holds every stage's `[tool_permissions]` (in stage
/// order); a call is judged by the map of the stage the world says the run is
/// in. The entry stage's `tool_accepts` seeds the limits a batch runs under,
/// and the pipeline's `sync_stage` swaps in the right ones as the agent
/// changes stage.
pub(crate) fn build_tool_state(parts: ToolStateParts<'_>) -> Arc<AgentToolState> {
    let entry_limits = parts
        .stage_tool_accepts_by_index
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
        stage_perms_by_index: Arc::new(parts.stage_perms_by_index),
        stage_required_by_index: Arc::new(parts.stage_required_by_index),
        stage_tool_accepts: Arc::new(StdMutex::new(entry_limits)),
        stage_tool_accepts_by_index: Arc::new(parts.stage_tool_accepts_by_index),
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
        yolo: crate::daemon::tool_service::Live::new(parts.yolo),
        protected: crate::daemon::tool_service::Live::new(parts.protected),
        stage_name: Arc::new(StdMutex::new(parts.entry_stage.to_string())),
        subagent: parts.subagent,
        sandbox: parts.sandbox,
        script_tools: Arc::new(StdMutex::new(parts.script_tools)),
        script_tool_names: Arc::new(StdMutex::new(parts.script_tool_names)),
        script_host: parts.script_host,
        offered_parts: parts.offered_parts,
        dynamic: parts.dynamic,
        // Everything a resume needs to redo this resolution against the config
        // as it stands then, rather than the copy this spawn read.
        config_source: Arc::new(crate::daemon::tool_service::ConfigSource {
            agent_name: parts.agent_name.to_string(),
            blueprint_safe: parts.blueprint_safe.cloned(),
            blueprint_read_paths: parts.blueprint_read_paths.to_vec(),
            workdir: parts.workdir,
            yolo_profile: parts.yolo_profile,
        }),
    })
}
