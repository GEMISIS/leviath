//! Binding a resolved run on the daemon: its compiled code, its sandbox and
//! its tool state.
//!
//! Hooks, validators and custom regions are compiled from the run file's own
//! code by the runtime. What is the daemon's own is the tool lane's view of
//! the run: built-in tools over the run's workdir behind the sandbox its
//! stages declare, the shared MCP connections, the script tools the run's
//! code holds, the permission layers (the run's `--allow`, each stage's and
//! the graph's `tool_permissions`, the operator's ceiling, the yolo profile),
//! and the sub-agent handle. That state is registered with the tool service
//! once the run's entity exists.

use std::collections::HashMap;

use leviath_core::policy::ToolPolicy;
use leviath_runtime::spec::env::{BindEnv, Bindings};
use leviath_runtime::spec::graph::SandboxDef;
use leviath_runtime::spec::launch::Unattended;
use leviath_runtime::spec::run_spec::{RunSpec, SpecOrigin};

use super::*;
use crate::daemon::sandbox_manager::SandboxManager;
use crate::daemon::tool_service::AgentToolState;

#[async_trait]
impl BindEnv for DaemonEnv {
    fn provider_fingerprint(&self, provider: &ProviderName) -> Option<Digest> {
        self.provider_print(provider)
    }

    fn mcp_fingerprint(&self, server: &McpServerName) -> Option<Digest> {
        self.mcp_print(server)
    }

    fn mcp_tools(&self, server: &McpServerName) -> Option<Vec<ToolDef>> {
        self.server_tools(server)
    }

    async fn bind(&self, spec: &RunSpec, code: &CodeFiles) -> Result<Bindings, SpawnIssues> {
        let bindings = leviath_runtime::bind::scripts::compile(spec, code)?;
        let state = self.tool_state(spec, code)?;
        let service = self.tool_service.clone();
        Ok(bindings.after_insert(move |entity| service.register(entity, state)))
    }
}

/// The name a run's permissions and grants are looked up under: its
/// blueprint's, or a raw graph's title.
pub(super) fn agent_name(spec: &RunSpec) -> String {
    match (&spec.origin, &spec.graph.title) {
        (SpecOrigin::Blueprint { blueprint, .. }, _) => blueprint.name.to_string(),
        (SpecOrigin::Raw, Some(title)) => title.clone(),
        (SpecOrigin::Raw, None) => "raw".to_string(),
    }
}

/// A graph's sandbox as the sandbox layer reads it.
fn sandbox_config(def: &SandboxDef) -> leviath_core::ToolSandboxConfig {
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

/// An issue at a top-level field of the spec.
fn at(field: &str, code: IssueCode, message: impl Into<String>) -> SpawnIssues {
    SpawnIssue::new(SpecPath::root().field(field), code, message).into()
}

impl DaemonEnv {
    /// The script tools the run's stages were given, compiled from the run
    /// file's own copy of their code.
    fn script_tools(
        &self,
        spec: &RunSpec,
        code: &CodeFiles,
    ) -> Result<(leviath_scripting::ScriptToolSet, HashSet<String>), SpawnIssues> {
        let mut set = leviath_scripting::ScriptToolSet::default();
        let mut names = HashSet::new();
        let mut issues = SpawnIssues::new();
        for plan in &spec.stages {
            let path = SpecPath::root()
                .field("stages")
                .key(plan.stage.as_str())
                .field("tools");
            for def in &plan.tools {
                let ToolSource::Script(digest) = &def.source else {
                    continue;
                };
                let text = code.get(digest).and_then(|b| std::str::from_utf8(b).ok());
                let added = text
                    .ok_or_else(|| {
                        format!("the run file holds no text for script tool '{}'", def.name)
                    })
                    .and_then(|t| {
                        set.add_source(&format!("run:{digest}"), t)
                            .map_err(|e| e.to_string())
                    });
                match added {
                    Ok(name) => {
                        names.insert(name);
                    }
                    Err(e) => issues.push(SpawnIssue::new(path.clone(), IssueCode::Invalid, e)),
                }
            }
        }
        issues.into_result((set, names))
    }

    /// The tool lane's state for a run.
    fn tool_state(
        &self,
        spec: &RunSpec,
        code: &CodeFiles,
    ) -> Result<Arc<AgentToolState>, SpawnIssues> {
        let config = &*self.config;
        let graph = &spec.graph;
        let workdir = spec.placement.workdir.clone();
        let agent = agent_name(spec);
        let run_id = spec.run_id.as_str();
        let mut issues = SpawnIssues::new();

        let (yolo, profile_name) = match &spec.launch.unattended {
            Unattended::Off => (false, None),
            Unattended::All => (true, None),
            Unattended::Profile(name) => (true, Some(name.to_string())),
        };
        let profile = issues.take(
            crate::yolo::resolve_for_spawn(yolo, profile_name.as_deref())
                .map_err(|e| at("launch", IssueCode::Unresolvable, e.to_string())),
        );
        let entry_index = graph
            .entry
            .as_ref()
            .and_then(|e| graph.stages.iter().position(|s| &s.name == e))
            .unwrap_or(0);
        let entry_stage = graph
            .stages
            .get(entry_index)
            .map(|s| s.name.to_string())
            .unwrap_or_default();
        let graph_sandbox = graph.sandbox.as_ref().map(sandbox_config);
        let by_index = graph
            .stages
            .iter()
            .map(|s| {
                leviath_core::resolve_sandbox(
                    config.sandbox.as_ref(),
                    graph_sandbox.as_ref(),
                    s.sandbox.as_ref().map(sandbox_config).as_ref(),
                )
            })
            .collect();
        let sandbox = issues.take(
            SandboxManager::build(run_id, by_index, &workdir.to_string_lossy(), entry_index)
                .map_err(|e| at("sandbox", IssueCode::Unavailable, e)),
        );
        let declared_reads = (!graph.read_paths.is_empty()).then(|| {
            leviath_runtime::spec::blueprint::ReadPathsConfig {
                allow: graph.read_paths.clone(),
            }
        });
        let reads = issues.take(
            crate::daemon::spawn::compile_read_path_policy(
                &agent,
                declared_reads.as_ref(),
                config,
                &workdir,
            )
            .map_err(|e| at("read_paths", IssueCode::Invalid, e)),
        );
        let scripts = issues.take(self.script_tools(spec, code));
        let (
            Some(profile),
            Some(sandbox),
            Some((reads, warning)),
            Some((script_tools, script_names)),
        ) = (profile, sandbox, reads, scripts)
        else {
            return Err(issues);
        };
        if let Some(line) = warning {
            tracing::warn!(agent_name = %agent, "{line}");
        }
        let sandbox = sandbox.map(Arc::new);

        let shell_env = crate::daemon::spawn::shell_env_policy(config);
        let tool_ctx = leviath_tools::ToolContext::new(workdir.clone())
            .with_read_paths(reads)
            .with_shell_env(shell_env.clone());
        let builtins =
            sandbox
                .iter()
                .fold(leviath_tools::BuiltinTools::new(tool_ctx), |tools, mgr| {
                    tools.with_shell_executor(mgr.clone() as Arc<dyn leviath_tools::ShellExecutor>)
                });
        let builtin_names: HashSet<String> = builtins.names().into_iter().collect();
        let builtins = Arc::new(
            builtins.with_reserved_names(
                crate::daemon::spawn::reserved_tool_names(&builtin_names, &self.mcp_defs)
                    .into_iter()
                    .collect(),
            ),
        );

        let stage_perms_by_index: Vec<HashMap<String, String>> = graph
            .stages
            .iter()
            .map(|s| permissions(&s.tool_permissions))
            .collect();
        let stage_required_by_index = graph
            .stages
            .iter()
            .map(|s| {
                s.required_tools
                    .iter()
                    .map(|n| leviath_tools::canonical_tool_name(n.as_str()).to_string())
                    .collect()
            })
            .collect();
        let stage_tool_accepts_by_index = graph
            .stages
            .iter()
            .map(|s| {
                s.tool_accepts
                    .iter()
                    .map(|(tool, list)| {
                        (
                            leviath_tools::canonical_tool_name(tool.as_str()).to_string(),
                            list.iter().map(ToString::to_string).collect(),
                        )
                    })
                    .collect()
            })
            .collect();
        let agent_perms = permissions(&graph.tool_permissions);
        let launch_overrides: HashMap<String, ToolPolicy> = spec
            .launch
            .allow
            .iter()
            .map(|t| (t.to_string(), ToolPolicy::Allow))
            .collect();

        let entry_perms = stage_perms_by_index
            .get(entry_index)
            .cloned()
            .unwrap_or_default();
        let agent_scoped = config.permissions_for_agent(&agent);
        let script_allow = crate::daemon::script_host::resolve_script_permissions(
            &config.tool_script_permissions,
            &|builtin| {
                let configured = crate::tools::resolve_policy(
                    builtin,
                    true,
                    &launch_overrides,
                    &entry_perms,
                    &agent_perms,
                    &agent_scoped,
                    config.security.allow_blueprint_permissions,
                );
                crate::yolo::apply_profile(
                    profile.as_deref(),
                    builtin,
                    &serde_json::Value::Null,
                    configured,
                    crate::tools::launch_allows(&launch_overrides, builtin),
                    crate::yolo::ToolKind::Builtin,
                    &workdir,
                )
            },
        );
        let writes = Arc::new(crate::daemon::tool_service::WriteBudget::new(
            config.limits.write_limits(),
        ));
        let offered_parts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let script_host: Arc<dyn leviath_scripting::ScriptHost> = Arc::new(
            crate::daemon::script_host::DaemonScriptHost::new(script_allow, workdir.clone())
                .with_write_budget(writes.clone())
                .with_shell(
                    sandbox.clone(),
                    std::time::Duration::from_secs(config.limits.script_shell_timeout_secs),
                    shell_env,
                )
                .with_local_network(config.security.allow_local_network)
                .with_env_allowlist(config.security.allow_env_vars.clone()),
        );
        let subagent = crate::daemon::subagent::SubAgentHandle {
            sender: self.subagent_tx.clone(),
            parent_run_id: run_id.to_string(),
            workdir: workdir.to_string_lossy().into_owned(),
            max_depth: usize::from(spec.launch.max_depth),
            no_seed_commands: !spec.launch.seed_commands,
            unattended: yolo,
            yolo_profile: profile_name.clone(),
            model_override: spec.requested_model.as_ref().map(ToString::to_string),
            offered_parts: offered_parts.clone(),
            mime: None,
        };
        let unattended = profile.as_ref().is_some_and(|p| p.spec.questions.is_auto());
        let safe = leviath_runtime::spec::blueprint::SafeCommandsConfig {
            tools: graph
                .safe_commands
                .tools
                .iter()
                .map(ToString::to_string)
                .collect(),
            shell: graph.safe_commands.shell.clone(),
        };
        let blueprint_safe = (safe != Default::default()).then_some(&safe);
        Ok(crate::daemon::spawn::build_tool_state(
            crate::daemon::spawn::ToolStateParts {
                writes,
                builtins,
                builtin_names,
                mcp: self.shared_mcp.clone(),
                config,
                hub: &self.hub,
                run_id,
                entry_stage: &entry_stage,
                entry_index,
                stage_perms_by_index,
                stage_required_by_index,
                stage_tool_accepts_by_index,
                agent_perms,
                agent_name: &agent,
                launch_overrides,
                subagent: Some(subagent),
                sandbox,
                script_tools,
                script_tool_names: script_names,
                script_host,
                offered_parts,
                dynamic: None,
                unattended,
                yolo: profile,
                yolo_profile: profile_name,
                protected: crate::tools::permission_files(config),
                blueprint_safe,
                blueprint_read_paths: declared_reads.as_ref(),
                workdir,
            },
        ))
    }
}

#[cfg(test)]
#[path = "bind_tests.rs"]
mod tests;
