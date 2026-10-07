//! The policies a run's tools and its spawn-time seeds both answer to.
//!
//! A seed can reach nothing the run could not reach mid-run, so the two are
//! built here once, from the run's graph and launch policy, and used by both:
//! the permission layers (the run's `allow`, the entry stage's and the graph's
//! `tool_permissions`, the operator's ceiling for this agent), the yolo
//! profile, the sandbox the stages declare, and what script tools may do.

use std::collections::HashMap;

use leviath_core::policy::ToolPolicy;
use leviath_runtime::spec::graph::{SandboxDef, ScriptPermission, ScriptPermissionsDef};
use leviath_runtime::spec::launch::{LaunchPolicy, Unattended};

use super::*;
use crate::config::{ScriptPermission as Configured, ScriptToolPermissions};
use crate::daemon::sandbox_manager::SandboxManager;
use crate::daemon::script_host::ScriptAllow;
use crate::yolo::YoloProfile;

/// A tool policy as the permission layers write it.
fn policy_word(policy: ToolPolicy) -> String {
    match policy {
        ToolPolicy::Allow => "allow",
        ToolPolicy::Ask => "ask",
        ToolPolicy::Deny => "deny",
    }
    .to_string()
}

fn permissions(
    table: &BTreeMap<leviath_runtime::spec::names::ToolName, ToolPolicy>,
) -> HashMap<String, String> {
    table
        .iter()
        .map(|(tool, policy)| (tool.to_string(), policy_word(*policy)))
        .collect()
}

/// The permission layers of one run.
#[derive(Debug, Clone)]
pub(super) struct Layers {
    /// The stage the run enters at, by position and by name.
    pub(super) entry_index: usize,
    pub(super) entry_stage: String,
    /// Each stage's `tool_permissions`, by position.
    pub(super) stage_perms_by_index: Vec<HashMap<String, String>>,
    /// The graph's `tool_permissions`.
    pub(super) agent_perms: HashMap<String, String>,
    /// The run's `allow` list.
    pub(super) launch_overrides: HashMap<String, ToolPolicy>,
    /// The operator's `[tool_permissions]` with this agent's grants on top.
    pub(super) agent_scoped: HashMap<String, ToolPolicy>,
    /// Whether a blueprint may loosen a tool below its built-in default.
    pub(super) may_loosen: bool,
}

impl Layers {
    pub(super) fn new(
        config: &Config,
        graph: &RunGraph,
        launch: &LaunchPolicy,
        agent: &str,
    ) -> Self {
        let entry_index = graph
            .entry
            .as_ref()
            .and_then(|e| graph.stages.iter().position(|s| &s.name == e))
            .unwrap_or(0);
        Self {
            entry_index,
            entry_stage: graph
                .stages
                .get(entry_index)
                .map(|s| s.name.to_string())
                .unwrap_or_default(),
            stage_perms_by_index: graph
                .stages
                .iter()
                .map(|s| permissions(&s.tool_permissions))
                .collect(),
            agent_perms: permissions(&graph.tool_permissions),
            launch_overrides: launch
                .allow
                .iter()
                .map(|t| (t.to_string(), ToolPolicy::Allow))
                .collect(),
            agent_scoped: config.permissions_for_agent(agent),
            may_loosen: config.security.allow_blueprint_permissions,
        }
    }

    /// The entry stage's own `tool_permissions`.
    pub(super) fn entry_perms(&self) -> HashMap<String, String> {
        self.stage_perms_by_index
            .get(self.entry_index)
            .cloned()
            .unwrap_or_default()
    }

    /// What the layers say about a call to `tool` from the entry stage,
    /// before any yolo profile has its say.
    pub(super) fn configured(&self, tool: &str, is_builtin: bool) -> ToolPolicy {
        crate::tools::resolve_policy(
            tool,
            is_builtin,
            &self.launch_overrides,
            &self.entry_perms(),
            &self.agent_perms,
            &self.agent_scoped,
            self.may_loosen,
        )
    }

    /// The layers, then the profile, for one call: what a seed's tool call
    /// and a script's `inherit` both answer to.
    pub(super) fn decide(
        &self,
        profile: Option<&YoloProfile>,
        tool: &str,
        arguments: &serde_json::Value,
        kind: crate::yolo::ToolKind,
        is_builtin: bool,
        workdir: &Path,
    ) -> ToolPolicy {
        crate::yolo::apply_profile(
            profile,
            tool,
            arguments,
            self.configured(tool, is_builtin),
            crate::tools::launch_allows(&self.launch_overrides, tool),
            kind,
            workdir,
        )
    }
}

/// The yolo profile a run decides its tool calls under: none for an attended
/// run, the built-in one for `all`, and the named profile read from
/// `yolo.toml` as it stands now.
pub(super) fn profile(
    unattended: &Unattended,
) -> Result<Option<Arc<YoloProfile>>, crate::yolo::YoloError> {
    crate::yolo::resolve_for_spawn(unattended)
}

/// A graph's sandbox as the sandbox layer reads it.
pub(super) fn sandbox_config(def: &SandboxDef) -> leviath_core::ToolSandboxConfig {
    leviath_core::ToolSandboxConfig {
        kind: def.kind,
        image: def.image.clone(),
        engine: def.engine.clone(),
        network: def.network,
        mounts: def.mounts.clone(),
        keep_warm: def.keep_warm,
        on_unavailable: def.on_unavailable,
    }
}

/// The sandbox `stage`'s shell runs in, cascading stage, graph, operator.
pub(super) fn stage_sandbox(
    config: &Config,
    graph: &RunGraph,
    stage: &StageDef,
) -> leviath_core::ToolSandboxConfig {
    leviath_core::resolve_sandbox(
        config.sandbox.as_ref(),
        graph.sandbox.as_ref().map(sandbox_config).as_ref(),
        stage.sandbox.as_ref().map(sandbox_config).as_ref(),
    )
}

/// The sandbox each stage's shell runs in (see [`stage_sandbox`]), starting
/// at the entry stage. `None` when no stage is sandboxed.
pub(super) fn sandbox(
    config: &Config,
    graph: &RunGraph,
    run_id: &str,
    workdir: &Path,
    entry_index: usize,
) -> Result<Option<SandboxManager>, String> {
    let by_index = graph
        .stages
        .iter()
        .map(|s| stage_sandbox(config, graph, s))
        .collect();
    SandboxManager::build(run_id, by_index, &workdir.to_string_lossy(), entry_index)
}

/// How strict a script permission is: `allow` least, `deny` most.
fn strictness(p: Configured) -> u8 {
    match p {
        Configured::Allow => 0,
        Configured::Inherit => 1,
        Configured::Deny => 2,
    }
}

fn configured(p: ScriptPermission) -> Configured {
    match p {
        ScriptPermission::Allow => Configured::Allow,
        ScriptPermission::Deny => Configured::Deny,
        ScriptPermission::Inherit => Configured::Inherit,
    }
}

/// The operator's script permissions with the graph's on top, field by
/// field, where the graph's is the stricter: a blueprint ships its own tool
/// scripts and may say they never need `shell`, never the opposite.
pub(super) fn script_permissions(
    global: &ScriptToolPermissions,
    graph: &ScriptPermissionsDef,
) -> ScriptToolPermissions {
    let tighten = |own: Option<ScriptPermission>, slot: Configured| match own.map(configured) {
        Some(p) if strictness(p) > strictness(slot) => p,
        _ => slot,
    };
    ScriptToolPermissions {
        http_get: tighten(graph.http_get, global.http_get),
        http_post: tighten(graph.http_post, global.http_post),
        shell: tighten(graph.shell, global.shell),
        read_file: tighten(graph.read_file, global.read_file),
        write_file: tighten(graph.write_file, global.write_file),
        env_var: tighten(graph.env_var, global.env_var),
    }
}

/// What the run's script tools may do: the script permissions resolved, an
/// `inherit` deferring to what the layers and the profile say about the
/// matching built-in.
pub(super) fn script_allow(
    config: &Config,
    graph: &RunGraph,
    layers: &Layers,
    profile: Option<&YoloProfile>,
    workdir: &Path,
) -> ScriptAllow {
    crate::daemon::script_host::resolve_script_permissions(
        &script_permissions(&config.tool_script_permissions, &graph.script_permissions),
        &|builtin| {
            layers.decide(
                profile,
                builtin,
                &serde_json::Value::Null,
                crate::yolo::ToolKind::Builtin,
                true,
                workdir,
            )
        },
    )
}

/// The graph's safe-command list as the approval layer reads it, when it
/// declares one.
pub(super) fn blueprint_safe(
    graph: &RunGraph,
) -> Option<leviath_runtime::spec::graph::SafeCommandsDef> {
    (graph.safe_commands != Default::default()).then(|| graph.safe_commands.clone())
}

/// Every script tool the run's code holds, compiled, with their names.
pub(super) fn code_tools(code: &CodeFiles) -> (leviath_scripting::ScriptToolSet, HashSet<String>) {
    let mut set = leviath_scripting::ScriptToolSet::default();
    let mut names = HashSet::new();
    for (digest, bytes) in code {
        let added = std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| set.add_source(&format!("run:{digest}"), text).ok());
        names.extend(added);
    }
    (set, names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_graph_can_only_tighten_what_its_scripts_may_do() {
        let global = ScriptToolPermissions {
            http_get: Configured::Allow,
            http_post: Configured::Inherit,
            shell: Configured::Deny,
            ..ScriptToolPermissions::default()
        };
        let graph = ScriptPermissionsDef {
            http_get: Some(ScriptPermission::Inherit),
            http_post: Some(ScriptPermission::Allow),
            shell: Some(ScriptPermission::Allow),
            read_file: Some(ScriptPermission::Deny),
            ..ScriptPermissionsDef::default()
        };
        let got = script_permissions(&global, &graph);
        assert_eq!(got.http_get, Configured::Inherit, "tightened");
        assert_eq!(got.http_post, Configured::Inherit, "never loosened");
        assert_eq!(got.shell, Configured::Deny, "never loosened");
        assert_eq!(got.read_file, Configured::Deny);
        assert_eq!(got.write_file, global.write_file, "left to the operator");
        assert_eq!(got.env_var, global.env_var);
    }
}
