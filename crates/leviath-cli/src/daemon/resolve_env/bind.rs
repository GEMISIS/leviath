//! Binding a resolved run on the daemon: everything about the run that is
//! this machine's to build.
//!
//! Hooks, validators and custom regions are compiled from the run file's own
//! code by the runtime, and so is the run's mime registry (this machine's
//! rows with the graph's on top, and the graph's checks). What is the daemon's
//! own:
//!
//! - the tool lane's view of the run: built-in tools over the run's workdir
//!   behind the sandbox its stages declare and with the run's blob store, the
//!   shared MCP connections, the script tools the run's code holds, the
//!   permission layers and the yolo profile, the sub-agent handle, and the
//!   re-scan context for a run whose tools are looked at again. It is
//!   registered with the tool service once the run's entity exists.
//! - the taint gate, with the operator's MCP reclassifications, and every
//!   tool's sensitivity, for a run under taint tracking;
//! - the chain of models a title call may walk, when the operator wants runs
//!   titled (insertion decides whether this run asks for one);
//! - what the operator grants of the run's `read_paths`, and where its
//!   blueprint lives, on the run's record.

use std::collections::HashMap;

use leviath_runtime::blob_store::RunMimeRegistry;
use leviath_runtime::spec::env::{BindEnv, Bindings};
use leviath_runtime::spec::graph::ToolRescan;
use leviath_runtime::spec::run_spec::{RunSpec, SpecOrigin};

use super::layers::{self, Layers};
use super::*;
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
        let mut issues = SpawnIssues::new();
        let compiled = issues.take(leviath_runtime::bind::scripts::compile(spec, code));
        let registry = issues.take(leviath_runtime::bind::scripts::mime_registry(
            spec, code, &self.mime,
        ));
        let state = issues.take(self.tool_state(spec, code, registry.as_ref()));
        let (Some(compiled), Some(registry), Some(state)) = (compiled, registry, state) else {
            return Err(issues);
        };
        let mut bindings = compiled.with(registry);
        bindings.extend(self.taint(spec, state.reads_granted));
        bindings.extend(self.title(spec));
        bindings.extend(self.record(spec));
        self.log_lint(spec);
        let service = self.tool_service.clone();
        let tools = state.tools;
        Ok(bindings.after_insert(move |entity| service.register(entity, tools)))
    }
}

/// Each warning and error `lev validate` reports for the blueprint in `dir`,
/// as one line naming the blueprint and the finding's code. Nothing for no
/// directory, or for one whose `agent.toml` will not read (resolving the run
/// already said why).
fn lint_lines(dir: Option<&Path>) -> Vec<String> {
    let Some(dir) = dir else {
        return Vec::new();
    };
    let file = std::fs::read_to_string(dir.join(leviath_blueprint::FILE_NAME))
        .ok()
        .and_then(|text| leviath_blueprint::BlueprintFile::parse(&text).ok());
    let Some(file) = file else {
        return Vec::new();
    };
    let env = crate::lint::LintEnv::offline(dir);
    crate::lint::lint_blueprint(&file, &env)
        .into_iter()
        .filter(|f| f.severity != crate::lint::LintSeverity::Note)
        .map(|f| {
            format!(
                "blueprint '{}': {} [{}]",
                file.blueprint.name,
                f.one_line(),
                f.code
            )
        })
        .collect()
}

/// The name a run's permissions and grants are looked up under: its
/// blueprint's, or a raw graph's title.
pub(super) fn agent_name(spec: &RunSpec) -> String {
    match (spec.origin.blueprint_name(), &spec.graph.title) {
        (Some(name), _) => name.to_string(),
        (None, Some(title)) => title.clone(),
        (None, None) => "raw".to_string(),
    }
}

/// An issue at a top-level field of the spec.
fn at(field: &str, code: IssueCode, message: impl Into<String>) -> SpawnIssues {
    SpawnIssue::new(SpecPath::root().field(field), code, message).into()
}

/// A bound run's tool state, and whether it may read outside its workdir.
struct ToolState {
    tools: Arc<AgentToolState>,
    reads_granted: bool,
}

impl DaemonEnv {
    /// The directory a blueprint run's blueprint is read from.
    fn blueprint_dir(&self, spec: &RunSpec) -> Option<PathBuf> {
        match &spec.origin {
            SpecOrigin::Blueprint { blueprint, .. } => self
                .agents_dir
                .as_ref()
                .map(|d| d.join(blueprint.name.as_str())),
            SpecOrigin::BlueprintFile { path, .. } => Some(path.path().to_path_buf()),
            SpecOrigin::Raw => None,
        }
    }

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

    /// What a run whose tools are looked at again needs to look: where, at
    /// what besides scripts, and what each stage may be given.
    fn rescan(
        &self,
        spec: &RunSpec,
        builtin_names: &HashSet<String>,
        builtins: &leviath_tools::BuiltinTools,
    ) -> Arc<crate::daemon::tool_service::DynamicToolCtx> {
        let graph = &spec.graph;
        let scan_dirs: Vec<PathBuf> = self
            .blueprint_dir(spec)
            .map(|d| d.join("tools"))
            .into_iter()
            .chain(std::iter::once(spec.placement.workdir.join("tools")))
            .chain(leviath_core::tools_dir())
            .collect();
        let mut static_defs = builtins.tool_defs();
        static_defs.extend(leviath_tools::BuiltinTools::subagent_tool_defs());
        static_defs.extend(self.mcp_defs.iter().cloned());
        let stamp = std::sync::Mutex::new(crate::daemon::tool_service::stamp_scan_dirs(&scan_dirs));
        Arc::new(crate::daemon::tool_service::DynamicToolCtx {
            scan_dirs,
            reserved_names: crate::daemon::spawn::reserved_tool_names(
                builtin_names,
                &self.mcp_defs,
            ),
            static_defs,
            mcp_owners: self.mcp_owners.clone(),
            stage_available: graph
                .stages
                .iter()
                .map(|s| host::stage_grants(s, &self.mcp_owners))
                .collect(),
            stage_required: graph
                .stages
                .iter()
                .map(|s| s.required_tools.iter().map(ToString::to_string).collect())
                .collect(),
            unattended: spec.auto_answers.questions,
            dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            stamp,
        })
    }

    /// The tool lane's state for a run, over `registry` (or, when the run's
    /// registry could not be built, over this machine's alone, so the
    /// tool state's own problems are reported beside it).
    fn tool_state(
        &self,
        spec: &RunSpec,
        code: &CodeFiles,
        registry: Option<&RunMimeRegistry>,
    ) -> Result<ToolState, SpawnIssues> {
        let config = &*self.config;
        let graph = &spec.graph;
        let workdir = spec.placement.workdir.clone();
        let agent = agent_name(spec);
        let run_id = spec.run_id.as_str();
        let mut issues = SpawnIssues::new();

        let profile = issues.take(
            layers::profile(&spec.launch.unattended)
                .map_err(|e| at("launch", IssueCode::Unresolvable, e.to_string())),
        );
        let layers = Layers::new(config, graph, &spec.launch, &agent);
        let sandbox = issues.take(
            layers::sandbox(config, graph, run_id, &workdir, layers.entry_index)
                .map_err(|e| at("sandbox", IssueCode::Unavailable, e)),
        );
        let reads = issues.take(
            crate::daemon::spawn::compile_read_path_policy(
                &agent,
                &graph.read_paths,
                config,
                &workdir,
            )
            .map_err(|e| at("read_paths", IssueCode::Invalid, e)),
        );
        let scripts = issues.take(self.script_tools(spec, code));
        let (
            Some((profile, profile_name)),
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
        let reads_granted =
            reads.is_active() && (reads.allow_blueprint || !reads.grants.is_empty());
        let sandbox = sandbox.map(Arc::new);
        let fallback;
        let registry = match registry {
            Some(registry) => registry,
            None => {
                fallback = RunMimeRegistry::new(&self.mime, toml::Table::new(), BTreeMap::new())
                    .expect("no rows of a run's own always layer");
                &fallback
            }
        };
        let mime = Arc::new(leviath_tools::ToolMime {
            store: self.blob_store.clone(),
            registry: registry.cell(),
            run_id: run_id.to_string(),
            max_part_bytes: config.max_part_bytes(),
        });

        let shell_env = crate::daemon::spawn::shell_env_policy(config);
        let tool_ctx = leviath_tools::ToolContext::new(workdir.clone())
            .with_read_paths(reads)
            .with_shell_env(shell_env.clone())
            .with_agent_tools_dir(self.blueprint_dir(spec).map(|d| d.join("tools")))
            .with_mime(mime.clone());
        let builtins =
            sandbox
                .iter()
                .fold(leviath_tools::BuiltinTools::new(tool_ctx), |tools, mgr| {
                    tools.with_shell_executor(mgr.clone() as Arc<dyn leviath_tools::ShellExecutor>)
                });
        let builtin_names: HashSet<String> = builtins.names().into_iter().collect();
        let builtins = builtins.with_reserved_names(
            crate::daemon::spawn::reserved_tool_names(&builtin_names, &self.mcp_defs)
                .into_iter()
                .collect(),
        );
        let dynamic = (graph.tool_rescan != ToolRescan::AtSpawn)
            .then(|| self.rescan(spec, &builtin_names, &builtins));
        let builtins = Arc::new(builtins);

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
        let script_allow =
            layers::script_allow(config, graph, &layers, profile.as_deref(), &workdir);
        let writes = Arc::new(crate::daemon::tool_service::WriteBudget::new(
            config.limits.write_limits(),
        ));
        let offered_parts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let script_host: Arc<dyn leviath_scripting::ScriptHost> = Arc::new(
            crate::daemon::script_host::DaemonScriptHost::new(script_allow, workdir.clone())
                .with_write_budget(writes.clone())
                .with_mime(mime.clone(), offered_parts.clone())
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
            no_seed_commands: !spec.launch.seed_commands,
            unattended: profile.is_some(),
            yolo_profile: profile_name.clone(),
            allow: spec.launch.allow.iter().map(ToString::to_string).collect(),
            model_override: spec.requested_model.as_ref().map(ToString::to_string),
            offered_parts: offered_parts.clone(),
            mime: Some(mime),
            agents_dir: self.agents_dir.clone(),
        };
        let safe = layers::blueprint_safe(graph);
        let Layers {
            entry_index,
            entry_stage,
            stage_perms_by_index,
            agent_perms,
            launch_overrides,
            ..
        } = layers;
        let tools = crate::daemon::spawn::build_tool_state(crate::daemon::spawn::ToolStateParts {
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
            dynamic,
            unattended: spec.auto_answers.questions,
            yolo: profile,
            yolo_profile: profile_name,
            protected: crate::tools::permission_files(config),
            blueprint_safe: safe.as_ref(),
            blueprint_read_paths: &graph.read_paths,
            workdir,
        });
        Ok(ToolState {
            tools,
            reads_granted,
        })
    }

    /// The taint gate and every tool's sensitivity, for a run under taint
    /// tracking (the resolver folded the operator's switch into the graph).
    /// The read tools count as private when the run may read outside its
    /// workdir.
    fn taint(&self, spec: &RunSpec, reads_granted: bool) -> Bindings {
        if spec.graph.taint_tracking != Some(true) {
            return Bindings::new();
        }
        let mut gate = leviath_runtime::TaintGate::new(leviath_core::taint::SecurityConfig {
            taint_tracking: true,
        });
        gate.apply_mcp_overrides(&self.mcp_overrides);
        let names = self
            .static_defs()
            .into_iter()
            .chain(spec.stages.iter().flat_map(|p| p.tools.iter().cloned()))
            .map(|t| t.name.to_string());
        let mut sensitivities: HashMap<String, leviath_core::TaintLevel> = names
            .map(|n| {
                let level = gate.tool_classification(&n).sensitivity;
                (n, level)
            })
            .collect();
        crate::daemon::spawn::bump_read_sensitivities(&mut sensitivities, reads_granted);
        Bindings::new().with((
            gate,
            leviath_runtime::pipeline::ToolSensitivities(sensitivities),
        ))
    }

    /// The models a title call may walk, best first: `[title]`'s choice,
    /// then the entry stage's model and its fallbacks. Nothing when the
    /// operator has titles off or no candidate writes text.
    fn title(&self, spec: &RunSpec) -> Bindings {
        let settings = &self.config.title;
        let entry = spec
            .graph
            .entry_stage()
            .and_then(|s| spec.stage(s.name.as_str()));
        let candidates = entry.map_or_else(Vec::new, |plan| {
            leviath_runtime::title::stage_pairs(
                plan.provider.as_str(),
                plan.model.as_str(),
                &plan.fallbacks,
            )
        });
        let label = spec
            .stages
            .first()
            .map(|p| format!("{}/{}", p.provider, p.model));
        let chain = leviath_runtime::title::title_chain(settings, label.as_deref(), &candidates);
        match settings.enabled && !chain.is_empty() {
            true => Bindings::new().with(leviath_runtime::title::TitleCandidates(chain)),
            false => Bindings::new(),
        }
    }

    /// Log, against the run, whatever `lev validate` would say about its
    /// blueprint. Nothing here refuses a run: these are authoring mistakes
    /// whose cost is a run that behaves oddly hours later, so the answer is
    /// put where someone looking for why a run stalled will find it. Notes
    /// describe what the blueprint means to do and are left out. A graph its
    /// caller wrote has no blueprint to lint.
    ///
    /// The lint is built from the blueprint's own directory so its
    /// `tools/*.rhai` resolve, and without the provider check: resolving the
    /// run already refused a stage no registered provider serves.
    fn log_lint(&self, spec: &RunSpec) {
        for line in lint_lines(self.blueprint_dir(spec).as_deref()) {
            tracing::warn!(run_id = %spec.run_id, "{line}");
        }
    }

    /// What only this machine knows about the run's record: how many of its
    /// `read_paths` the operator grants, and where its blueprint is installed.
    fn record(&self, spec: &RunSpec) -> Bindings {
        let counts = crate::read_path_report::build_declared(
            &agent_name(spec),
            &spec.graph.read_paths,
            &self.config,
            &spec.placement.workdir,
        )
        .and_then(Result::ok)
        .map(|report| leviath_core::run_meta::ReadPathGrantCounts {
            declared: report.declared(),
            granted: report.granted(),
        });
        let path = self
            .blueprint_dir(spec)
            .map(|d| d.join(leviath_blueprint::FILE_NAME))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        Bindings::new().edit(
            move |meta: &mut leviath_runtime::persistence::RunMetadata| {
                meta.read_paths = counts;
                meta.agent_path = path;
            },
        )
    }
}

#[cfg(test)]
#[path = "bind_tests.rs"]
mod tests;
