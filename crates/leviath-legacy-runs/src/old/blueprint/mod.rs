//! An old `agent.leviath` manifest as its parser reads it: stages, model
//! choices, tool grants, edges and the context layout, with names kept as
//! plain text. [`from_blueprint`](crate::old::graph::from_blueprint) reads one
//! as a run graph.

use crate::old::layout::ContextLayout;
use leviath_core::lifecycle::CompactionConfig;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// When a run looks for tools again after it started.
///
/// Discovery happens either way: what this decides is whether it happens more
/// than once, and how eagerly. Each value is strictly more eager than the one
/// before it, so a later value does everything an earlier one does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRescan {
    /// The set is fixed when the run starts. The default, and the only value
    /// where an agent cannot grow its own toolchain.
    #[default]
    AtSpawn,
    /// A `.rhai` written into a scanned directory makes the run look again
    /// before its next turn, so the tool is advertised to the model.
    AfterWrites,
    /// As `AfterWrites`, and the run also looks at the scanned directories
    /// themselves before each batch of tool calls it dispatches.
    ///
    /// The difference is *what* it notices. `AfterWrites` is told about a tool
    /// only when this agent writes one with `write_file`, `edit_file` or
    /// `install_tool`. A tool that appears any other way - written by a shell
    /// command, by a script tool, by a sub-agent or fan-out worker sharing this
    /// workdir, or by a person - is invisible to it for the rest of the run.
    /// This value looks at the directories instead of waiting to be told, so it
    /// sees all of those, and sees a tool that was edited or removed too.
    ///
    /// The cost is a `stat` per scanned directory per batch, and a re-scan only
    /// when one of them changed.
    BeforeDispatch,
}

impl ToolRescan {
    /// The word a manifest writes for this value.
    pub fn wire(self) -> &'static str {
        match self {
            Self::AtSpawn => "at_spawn",
            Self::AfterWrites => "after_writes",
            Self::BeforeDispatch => "before_dispatch",
        }
    }

    /// Read a manifest's word, or `None` for one nothing here names.
    pub fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "at_spawn" => Self::AtSpawn,
            "after_writes" => Self::AfterWrites,
            "before_dispatch" => Self::BeforeDispatch,
            _ => return None,
        })
    }

    /// Every value, in order of eagerness, for a refusal that lists them.
    pub const ALL: [Self; 3] = [Self::AtSpawn, Self::AfterWrites, Self::BeforeDispatch];
}

/// An agent blueprint - the complete definition of an agent type.
///
/// Includes stages, model selection, tools, AND context layout. A blueprint
/// defines everything needed to instantiate and run an agent with specific
/// capabilities and memory structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blueprint {
    /// Unique name for this agent type
    pub name: String,

    /// Human-readable description
    pub description: String,

    /// Execution stages (e.g., analyze → implement → review)
    pub stages: Vec<Stage>,

    /// Context window layout defining memory regions
    pub context_layout: ContextLayout,

    /// Context transforms for inter-agent communication
    pub transforms: Vec<ContextTransform>,

    /// Version of this blueprint
    pub version: String,

    /// Configuration for LLM-based compaction
    pub compaction_config: Option<CompactionConfig>,

    /// Maximum depth of the sub-agent tree (default: 3)
    pub max_child_depth: Option<usize>,

    /// Which stage to start from (default: first defined)
    pub entry_stage: Option<String>,

    /// Additional metadata
    pub metadata: HashMap<String, serde_json::Value>,

    /// Security configuration for taint tracking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<leviath_core::taint::SecurityConfig>,

    /// Agent-level override for the batch-tool-calls system-prompt hint. `None`
    /// inherits the global config toggle; a per-stage `batch_tool_hint` overrides
    /// this. See [`leviath_core::taint::resolve_batch_tool_hint`] for the cascade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_tool_hint: Option<bool>,

    /// Agent-level override for the platform shell hint. `None` inherits the
    /// global config toggle; a per-stage `shell_hint` overrides this. See
    /// [`leviath_core::taint::resolve_shell_hint`] for the cascade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_hint: Option<bool>,

    /// Agent-level default for the empty-response nudge. `None` inherits the
    /// global config's `[nudge]` section; a per-stage `[stages.<name>.nudge]`
    /// overrides this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nudge: Option<NudgeConfig>,

    /// Repetition detection configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repetition_detection: Option<RepetitionDetectionConfig>,

    /// File tracking configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_tracking: Option<FileTrackingConfig>,

    /// Agent-level sandbox configuration for tool execution. Per-stage
    /// `[stages.<name>.sandbox]` overrides this; both cascade through
    /// [`leviath_core::resolve_sandbox`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<leviath_core::sandbox::ToolSandboxConfig>,

    /// When a run looks for tools again after it started.
    ///
    /// Anything but [`ToolRescan::AtSpawn`] puts the run workdir's `tools/`
    /// directory in the scan set, so a script the agent writes there mid-run
    /// can be found. The directory is the *workdir's*, not the blueprint's:
    /// anything else running in that workdir sees the same tools, and a
    /// sub-agent inherits the workdir verbatim.
    ///
    /// Defaults to [`ToolRescan::AtSpawn`], where the set is fixed when the run
    /// starts and an agent cannot grow its own toolchain.
    #[serde(default)]
    pub tool_rescan: ToolRescan,

    /// Read paths this agent *declares* beyond its workdir - directories a
    /// planner-style agent needs to see, like run archives or design docs.
    /// Declaring is not granting: entries only take effect when the user's
    /// config also grants them (`[security] read_paths`,
    /// `[agent_read_paths.<name>]`, or `allow_blueprint_read_paths = true`),
    /// so an installed manifest cannot widen its own sandbox. Read-only in
    /// every case; `write_file` and `edit_file` stay confined to the workdir.
    /// Semantics live in [`leviath_core::read_paths`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_paths: Option<ReadPathsConfig>,

    /// The `[safe_commands]` section: tools and shell command prefixes this
    /// agent would like to run without an approval prompt.
    ///
    /// Declaring is not granting, exactly as for [`Self::read_paths`]: entries
    /// take effect only when the user opts in, per agent via
    /// `[agent_safe_commands.<name>] allow_blueprint = true` or globally via
    /// `[security] allow_blueprint_safe_commands`. Otherwise any agent package
    /// could pre-approve its own shell with one TOML line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_commands: Option<SafeCommandsConfig>,

    /// Agent-level default shape for the run's final output. A per-stage
    /// `[stages.<name>.output]` narrows it, and whoever starts the run can
    /// override it again. See [`leviath_core::output::resolve_output_spec`].
    ///
    /// `None` means this agent declares no shape, which is not the same as
    /// producing no output: a stage may still ask for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<leviath_core::output::OutputSpec>,

    /// Rows this agent adds to the mime registry, `[mime_types]` in the
    /// manifest: the types its tools produce and take, layered over the
    /// operator's rows for this agent's runs only. Validated at parse; an
    /// empty table is the common case and is not written back.
    #[serde(default, skip_serializing_if = "toml::Table::is_empty")]
    pub mime_types: toml::Table,

    /// Things that must be in place before this agent can run, declared as
    /// `[[dependencies]]` in the manifest: an MCP server, an environment
    /// variable, a program on `PATH`, or a condition a Rhai script checks.
    /// Declared, never granted. The operator is shown what is missing and how
    /// to fix it, and an unmet required dependency fails the spawn before the
    /// first billed inference. See [`Dependency`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<Dependency>,
}

/// The `[safe_commands]` section of a manifest.
///
/// Entry syntax is not checked here. What counts as a usable shell prefix is
/// defined by the key parser in the CLI (a program, optionally with the
/// subcommand that narrows it), which this crate does not depend on. A bad
/// entry is a lint finding and is skipped with a warning at spawn, rather than
/// a parse error - the same place the check can be written once instead of
/// twice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafeCommandsConfig {
    /// Tools that need no prompt whatever their arguments.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Shell command prefixes that need no prompt: `"cargo test"`, not
    /// `"cargo test --lib"` and never `"cargo"`.
    #[serde(default)]
    pub shell: Vec<String>,
}

/// The `[read_paths]` section of a manifest: raw declared entries, compiled
/// against the run's workdir and home at spawn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadPathsConfig {
    /// Declared entries. Each may be:
    /// - an exact path, granting its subtree: `"~/.leviath/runs"` or
    ///   `"../shared-docs"` (relative to the run's workdir)
    /// - a glob: `"glob:~/.leviath/runs/**"`
    /// - a regex, auto-anchored: `"regex:/data/design-docs/.*"`
    ///
    /// Patterns are written with `/` separators on every OS and match the
    /// symlink-resolved real path.
    #[serde(default)]
    pub allow: Vec<String>,
}

/// One `[[dependencies]]` entry: something that must be in place before an
/// agent can run. Declared in the manifest, never granted - every surface that
/// reports it (`lev validate`, `lev deps`, the spawn gate, the API) shows what
/// is missing and the `remedy` for fixing it.
///
/// The `kind` field selects what must be present and carries its own fields
/// (see [`DependencyKind`]); the optional [`install`](Self::install) block says
/// how `lev deps install` can put it in place, and is never run automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    /// A short identifier, unique within the blueprint.
    pub name: String,

    /// What must be present, and the fields describing it.
    #[serde(flatten)]
    pub kind: DependencyKind,

    /// Whether an unmet dependency blocks the run. `true` (the default) fails
    /// the spawn; `false` downgrades a miss to a warning the run proceeds past.
    #[serde(default = "default_dependency_required")]
    pub required: bool,

    /// A human sentence telling the user how to satisfy the dependency, shown
    /// wherever a miss is reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,

    /// A one-line note on why the agent needs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// How `lev deps install` can put this dependency in place. Optional and
    /// never run automatically: installing runs commands or writes config on
    /// the user's machine and always asks first. See [`DependencyInstall`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<DependencyInstall>,
}

/// The default for [`Dependency::required`]: a declared dependency blocks the
/// run unless the manifest says otherwise.
fn default_dependency_required() -> bool {
    true
}

/// What a [`Dependency`] requires, selected by the manifest's `kind` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DependencyKind {
    /// An MCP server that must be configured in the user's config, plus any
    /// environment variables or secrets it needs. The check confirms the named
    /// server exists and every `env` var is set and non-empty.
    McpServer {
        /// The server name that must appear in the user's `[[mcp_servers]]`.
        server: String,
        /// Environment variables / secrets the server needs. Values are
        /// prompted for at install, never stored in the blueprint.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env: Vec<String>,
    },
    /// An environment variable that must be set and non-empty.
    Env {
        /// The variable name.
        var: String,
    },
    /// A program that must resolve on `PATH`.
    Binary {
        /// The program name, e.g. `blender`.
        command: String,
    },
    /// A condition a Rhai script decides. The `check` script returns
    /// `#{ ok: bool, remedy: string }`; the optional installer lives in
    /// [`DependencyInstall::script`].
    Script {
        /// Path to the Rhai check script, relative to the blueprint directory.
        check: String,
    },
}

impl DependencyKind {}

/// How a [`Dependency`] can be installed by `lev deps install`.
///
/// Every field is optional; a dependency may declare any combination. Nothing
/// here runs without an explicit `lev deps install` and a confirmation, because
/// each option changes the user's machine: running a command, executing a
/// script, or writing an MCP server into their config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyInstall {
    /// A shell command that installs the dependency on any platform, e.g.
    /// `"pip install trimesh"`. Run only after the user confirms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    /// Per-OS shell commands, keyed by `"macos"`, `"linux"` or `"windows"`,
    /// preferred over [`command`](Self::command) on a matching host.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub commands: BTreeMap<String, String>,

    /// A Rhai install script (relative to the blueprint), run with the script
    /// I/O surface and gated exactly like a script tool. For a `script`
    /// dependency this is its installer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,

    /// For an `mcp_server` dependency: the non-user-specific server settings the
    /// installer writes into the user's config. Secrets are never placed here -
    /// they are named in the dependency's `env` and prompted for securely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<McpServerTemplate>,
}

/// The non-secret settings for an MCP server that a blueprint can ship so
/// `lev deps install` can write it into the user's config. Mirrors the
/// installable half of the CLI's MCP server config; the user-specific secrets
/// (header and env values) are prompted for and stored separately.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerTemplate {
    /// `"stdio"` or `"http"`. Inferred from `command`/`url` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// The program to launch for a stdio server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The endpoint for an http server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Arguments passed to `command`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Non-secret headers, for an http server. A value may reference a secret
    /// with `${VAR}`, where `VAR` is named in the dependency's `env`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Environment for a stdio server's child process. A value may reference a
    /// secret with `${VAR}` (expanded from the environment at connect time, so
    /// the secret stays out of the config file), where `VAR` is named in the
    /// dependency's `env`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

impl Blueprint {
    /// Create a new blueprint with the specified configuration.
    pub fn new(
        name: String,
        description: String,
        stages: Vec<Stage>,
        context_layout: ContextLayout,
    ) -> Self {
        Self {
            name,
            description,
            stages,
            context_layout,
            transforms: Vec::new(),
            version: "0.1.0".to_string(),
            compaction_config: None,
            max_child_depth: None,
            entry_stage: None,
            metadata: HashMap::new(),
            security: None,
            batch_tool_hint: None,
            shell_hint: None,
            nudge: None,
            repetition_detection: None,
            file_tracking: None,
            sandbox: None,
            tool_rescan: ToolRescan::AtSpawn,
            read_paths: None,
            safe_commands: None,
            output: None,
            mime_types: toml::Table::new(),
            dependencies: Vec::new(),
        }
    }

    /// Agent-level tool permissions, keyed by tool name.
    ///
    /// The manifest parser records a top-level `[tool_permissions]` block as
    /// `tool_perm:<tool>` → policy-string entries in [`Self::metadata`]. This
    /// projects them back into a tool-keyed map, the graph's run-wide
    /// `tool_permissions`. Non-`tool_perm:` keys and non-string values are ignored.
    pub fn agent_tool_permissions(&self) -> HashMap<String, String> {
        self.metadata
            .iter()
            .filter_map(|(k, v)| {
                Some((
                    k.strip_prefix("tool_perm:")?.to_string(),
                    v.as_str()?.to_string(),
                ))
            })
            .collect()
    }
}

// Sections of the former single-file blueprint, one per concept. Glob
// re-exported so every existing `blueprint::Stage` path keeps working and the
// split stays a pure move.
mod model;
pub use model::*;
mod stage;
pub use stage::*;
mod transition;
pub use transition::*;
mod tool_groups;
pub use tool_groups::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::old::layout::ContextLayout;
    use crate::old::layout::RegionDefinition;
    use leviath_core::region::RegionKind;

    #[test]
    fn test_blueprint_creation() {
        let regions = vec![RegionDefinition::new(
            "test".to_string(),
            RegionKind::Pinned,
            5000,
        )];
        let layout = ContextLayout::new(regions, 10000);

        let stages = vec![Stage::new(
            "analyze".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
        )];

        let blueprint = Blueprint::new(
            "test-agent".to_string(),
            "A test agent".to_string(),
            stages,
            layout,
        );

        assert_eq!(blueprint.name, "test-agent");
        assert_eq!(blueprint.stages.len(), 1);
    }

    #[test]
    fn agent_tool_permissions_projects_only_string_tool_perm_entries() {
        let stages = vec![Stage::new("plan".to_string(), make_model())];
        let mut bp = Blueprint::new("t".into(), "d".into(), stages, make_layout());
        // A well-formed tool_perm string entry - included.
        bp.metadata.insert(
            "tool_perm:bash".to_string(),
            serde_json::Value::String("deny".to_string()),
        );
        // A non-`tool_perm:` key - skipped (strip_prefix returns None).
        bp.metadata
            .insert("title".to_string(), serde_json::Value::String("x".into()));
        // A tool_perm key whose value isn't a string - skipped (as_str is None).
        bp.metadata
            .insert("tool_perm:weird".to_string(), serde_json::Value::Bool(true));

        let perms = bp.agent_tool_permissions();
        assert_eq!(perms.get("bash").map(String::as_str), Some("deny"));
        assert!(!perms.contains_key("title"));
        assert!(!perms.contains_key("weird"));
        assert_eq!(perms.len(), 1);
    }

    #[test]
    fn test_stage_with_mode() {
        let stage = Stage::new("test".to_string(), make_model())
            .with_mode(StageMode::InteractivePoints { points: vec![] });
        assert_eq!(stage.mode, StageMode::InteractivePoints { points: vec![] });
    }

    #[test]
    fn test_stage_allow_complete_defaults_false() {
        let stage = Stage::new("review".to_string(), make_model());
        assert!(!stage.allow_complete);
    }

    #[test]
    fn test_stage_allow_complete_serde_default_when_missing() {
        // A serialized stage from before allow_complete existed must still
        // deserialize, defaulting to false.
        let json = r#"{
            "name": "review",
            "description": null,
            "model": {"provider": "anthropic", "model": "claude-sonnet-4-6", "parameters": {}},
            "available_tools": [],
            "max_iterations": null,
            "context_layout": null,
            "config": {},
            "transitions": null,
            "max_revisits": null,
            "transition_prompt": null
        }"#;
        let stage: Stage = serde_json::from_str(json).unwrap();
        assert!(!stage.allow_complete);
        assert!(stage.accepts_messages);
    }

    #[test]
    fn test_stage_allow_complete_roundtrip() {
        let mut stage = Stage::new("review".to_string(), make_model());
        stage.allow_complete = true;
        let json = serde_json::to_string(&stage).unwrap();
        let back: Stage = serde_json::from_str(&json).unwrap();
        assert!(back.allow_complete);
    }

    #[test]
    fn test_interaction_point_directives_default_empty() {
        let point = InteractionPoint {
            name: "plan_approval".to_string(),
            prompt: "Approve?".to_string(),
            required: true,
            unattended: UnattendedPolicy::AutoApprove,
            style: InteractionStyle::MultipleChoice,
            options: vec!["Approve".to_string(), "Revise".to_string()],
            directives: HashMap::new(),
            abort_options: Vec::new(),
            edit_options: Vec::new(),
            document_region: None,
        };
        assert!(point.directives.is_empty());
        assert!(point.abort_options.is_empty());
        assert!(point.edit_options.is_empty());
    }

    #[test]
    fn test_interaction_point_directives_roundtrip() {
        let mut directives = HashMap::new();
        directives.insert(
            "Revise".to_string(),
            "Ask what to change, then re-plan.".to_string(),
        );
        let point = InteractionPoint {
            name: "plan_approval".to_string(),
            prompt: "Approve?".to_string(),
            required: true,
            unattended: UnattendedPolicy::Ask,
            style: InteractionStyle::MultipleChoice,
            options: vec!["Approve".to_string(), "Revise".to_string()],
            directives,
            abort_options: vec!["Abort".to_string()],
            edit_options: vec!["Add detail".to_string()],
            document_region: Some("plan".to_string()),
        };
        let json = serde_json::to_string(&point).unwrap();
        let back: InteractionPoint = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.directives.get("Revise").map(|s| s.as_str()),
            Some("Ask what to change, then re-plan.")
        );
        assert_eq!(back.abort_options, vec!["Abort".to_string()]);
        assert_eq!(back.edit_options, vec!["Add detail".to_string()]);
        // A point that holds for a person under `--yolo` has to survive the
        // round trip: this is what a restored run re-arms from.
        assert_eq!(back.unattended, UnattendedPolicy::Ask);
    }

    #[test]
    fn test_interaction_point_directives_serde_default_when_missing() {
        let json = r#"{
            "name": "plan_approval",
            "prompt": "Approve?",
            "required": true,
            "style": "multiple_choice",
            "options": ["Approve", "Revise"]
        }"#;
        let point: InteractionPoint = serde_json::from_str(json).unwrap();
        assert!(point.directives.is_empty());
        assert!(point.abort_options.is_empty());
    }

    #[test]
    fn test_interaction_point_followups_alias_still_deserializes() {
        // Backward compat: old serialized blueprints used "followups".
        let json = r#"{
            "name": "plan_approval",
            "prompt": "Approve?",
            "required": true,
            "style": "multiple_choice",
            "options": ["Approve", "Revise"],
            "followups": { "Revise": "What to change?" }
        }"#;
        let point: InteractionPoint = serde_json::from_str(json).unwrap();
        assert_eq!(
            point.directives.get("Revise").map(|s| s.as_str()),
            Some("What to change?")
        );
    }

    #[test]
    fn test_model_config_new_creates_single_entry() {
        let mc = ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string());
        assert_eq!(mc.models.len(), 1);
        assert_eq!(mc.models[0].provider, "anthropic");
        assert_eq!(mc.models[0].model, "claude-sonnet-4-6");
        assert!(mc.allow_user_default);
    }

    #[test]
    fn test_model_config_with_multiple_models() {
        let mc = ModelConfig {
            models: vec![
                ModelEntry::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
                ModelEntry::new("openai".to_string(), "gpt-4o".to_string()),
                ModelEntry::new("ollama".to_string(), "llama3".to_string()),
            ],
            allow_user_default: true,
            parameters: HashMap::new(),
            request_timeout_secs: None,
        };
        assert_eq!(mc.models.len(), 3);
        assert_eq!(mc.models[0].provider, "anthropic");
        assert_eq!(mc.models[1].provider, "openai");
        assert_eq!(mc.models[2].provider, "ollama");
    }

    #[test]
    fn test_model_config_serde_roundtrip() {
        let mc = ModelConfig {
            models: vec![
                ModelEntry::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
                ModelEntry::new("openai".to_string(), "gpt-4o".to_string()),
            ],
            allow_user_default: false,
            parameters: HashMap::new(),
            request_timeout_secs: None,
        };
        let json = serde_json::to_string(&mc).unwrap();
        let back: ModelConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.models.len(), 2);
        assert_eq!(back.models[0].provider, "anthropic");
        assert_eq!(back.models[1].provider, "openai");
        assert!(!back.allow_user_default);
    }

    #[test]
    fn test_model_config_serde_defaults_when_fields_missing() {
        // Minimal JSON - models defaults to empty, allow_user_default defaults to true
        let json = r#"{"parameters": {}}"#;
        let mc: ModelConfig = serde_json::from_str(json).unwrap();
        assert!(mc.models.is_empty());
        assert!(mc.allow_user_default);
    }

    fn make_model() -> ModelConfig {
        ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string())
    }

    fn make_layout() -> ContextLayout {
        let regions = vec![RegionDefinition::new(
            "test".to_string(),
            RegionKind::Pinned,
            5000,
        )];
        ContextLayout::new(regions, 10000)
    }

    #[test]
    fn test_transition_condition_default() {
        let cond = TransitionCondition::default();
        assert_eq!(cond, TransitionCondition::Always);
    }

    #[test]
    fn test_edge_transform_default() {
        let t = EdgeTransform::default();
        assert_eq!(t, EdgeTransform::Direct);
    }

    #[test]
    fn test_stage_mode_equality() {
        assert_eq!(StageMode::Autonomous, StageMode::Autonomous);
        assert_eq!(StageMode::Interactive, StageMode::Interactive);
        assert_ne!(StageMode::Autonomous, StageMode::Interactive);
    }

    #[test]
    fn test_interaction_style_equality() {
        assert_eq!(InteractionStyle::FreeText, InteractionStyle::FreeText);
        assert_ne!(InteractionStyle::FreeText, InteractionStyle::MultipleChoice);
    }

    // ─── stuck detection ────────────────────────────────────────────────────

    #[test]
    fn stuck_config_is_armed_only_when_a_threshold_is_set() {
        assert!(!StuckConfig::default().is_armed());
        for cfg in [
            StuckConfig {
                after_iterations: Some(1),
                ..Default::default()
            },
            StuckConfig {
                after_minutes: Some(1),
                ..Default::default()
            },
            StuckConfig {
                after_same_file_edits: Some(1),
                ..Default::default()
            },
            StuckConfig {
                after_tool_calls: Some(1),
                ..Default::default()
            },
        ] {
            assert!(cfg.is_armed(), "{cfg:?} should be armed");
        }
    }

    #[test]
    fn transition_condition_stuck_round_trips_as_snake_case() {
        let json = serde_json::to_string(&TransitionCondition::Stuck).unwrap();
        assert_eq!(json, "\"stuck\"");
        let back: TransitionCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(back, TransitionCondition::Stuck);
        assert_ne!(TransitionCondition::Stuck, TransitionCondition::Always);
    }

    #[test]
    fn transition_edge_stuck_round_trips_and_is_omitted_when_absent() {
        let plain = TransitionEdge {
            target: "b".to_string(),
            condition: TransitionCondition::Always,
            hint: None,
            transform: EdgeTransform::Direct,
            gate: None,
            stuck: None,
        };
        let json = serde_json::to_string(&plain).unwrap();
        assert!(
            !json.contains("stuck"),
            "absent config must be skipped: {json}"
        );

        let armed = TransitionEdge {
            condition: TransitionCondition::Stuck,
            stuck: Some(StuckConfig {
                after_iterations: Some(20),
                after_minutes: Some(10),
                after_same_file_edits: Some(3),
                after_tool_calls: Some(60),
            }),
            ..plain
        };
        let back: TransitionEdge = serde_json::from_str(&serde_json::to_string(&armed).unwrap())
            .expect("armed edge round-trips");
        assert_eq!(back.condition, TransitionCondition::Stuck);
        assert_eq!(back.stuck, armed.stuck);
    }

    #[test]
    fn output_mode_compares_equal_only_to_itself() {
        assert_eq!(StageMode::Output, StageMode::Output);
        assert_ne!(StageMode::Output, StageMode::Autonomous);
        assert_ne!(StageMode::Autonomous, StageMode::Output);
    }

    #[test]
    fn test_transition_condition_equality() {
        assert_eq!(
            TransitionCondition::LlmChoice,
            TransitionCondition::LlmChoice
        );
        assert_ne!(TransitionCondition::Always, TransitionCondition::Error);
    }

    #[test]
    fn test_edge_transform_compact_and_custom_equality() {
        let a = EdgeTransform::Compact {
            prompt: Some("p".to_string()),
        };
        let b = EdgeTransform::Compact {
            prompt: Some("p".to_string()),
        };
        assert_eq!(a, b);

        let c1 = EdgeTransform::Custom {
            carry: vec!["a".to_string()],
            compact: vec!["b".to_string()],
            clear: vec!["c".to_string()],
            compact_prompt: Some("p".to_string()),
        };
        let c2 = c1.clone();
        assert_eq!(c1, c2);

        assert_ne!(EdgeTransform::Direct, EdgeTransform::Clear);
    }

    #[test]
    fn test_stage_accepts_messages_default_true() {
        let stage = Stage::new(
            "test".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
        );
        assert!(stage.accepts_messages);
    }

    #[test]
    fn test_stage_accepts_messages_serde_roundtrip() {
        // Serialize a stage with accepts_messages = false, then deserialize
        let mut stage = Stage::new(
            "report".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-opus-4-6".to_string()),
        );
        stage.accepts_messages = false;

        let json = serde_json::to_string(&stage).expect("should serialize");
        let deserialized: Stage = serde_json::from_str(&json).expect("should deserialize");
        assert!(!deserialized.accepts_messages);
    }

    #[test]
    fn test_stage_accepts_messages_json_default() {
        // When accepts_messages is missing from JSON, it should default to true
        let json = r#"{
            "name": "analyze",
            "model": { "provider": "anthropic", "model": "claude-sonnet-4-6", "parameters": {} },
            "available_tools": [],
            "mode": "Autonomous",
            "config": {},
            "tool_permissions": {},
            "requires_children": false
        }"#;
        let stage: Stage = serde_json::from_str(json).expect("should parse");
        assert!(stage.accepts_messages);
    }

    #[test]
    fn test_file_tracking_config_defaults() {
        let json = r#"{"region": "files"}"#;
        let config: FileTrackingConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.region, "files");
        assert!(config.track_reads);
        assert!(config.track_writes);
        assert!(config.max_file_tokens.is_none());
    }

    #[test]
    fn test_file_tracking_config_serde_roundtrip() {
        let config = FileTrackingConfig {
            region: "files".to_string(),
            track_reads: true,
            track_writes: false,
            max_file_tokens: Some(5000),
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: FileTrackingConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.region, "files");
        assert!(back.track_reads);
        assert!(!back.track_writes);
        assert_eq!(back.max_file_tokens, Some(5000));
    }

    #[test]
    fn test_blueprint_file_tracking_default_none() {
        let stages = vec![Stage::new("plan".to_string(), make_model())];
        let bp = Blueprint::new("t".into(), "d".into(), stages, make_layout());
        assert!(bp.file_tracking.is_none());
    }

    #[test]
    fn test_blueprint_file_tracking_serde_roundtrip() {
        let stages = vec![Stage::new("plan".to_string(), make_model())];
        let mut bp = Blueprint::new("t".into(), "d".into(), stages, make_layout());
        bp.file_tracking = Some(FileTrackingConfig {
            region: "files".to_string(),
            track_reads: true,
            track_writes: true,
            max_file_tokens: Some(3000),
        });
        let json = serde_json::to_string(&bp).unwrap();
        let back: Blueprint = serde_json::from_str(&json).unwrap();
        let ft = back.file_tracking.unwrap();
        assert_eq!(ft.region, "files");
        assert_eq!(ft.max_file_tokens, Some(3000));
    }

    #[test]
    fn test_tool_result_routing_default() {
        let routing = ToolResultRouting::default();
        assert_eq!(routing.default_region, "tool_results");
        assert!(routing.keep_results);
        assert!(routing.tool_overrides.is_empty());
        assert!(routing.max_result_tokens.is_none());
    }

    #[test]
    fn test_stage_new_has_no_tool_result_routing() {
        let stage = Stage::new("plan".to_string(), make_model());
        assert!(stage.tool_result_routing.is_none());
    }

    #[test]
    fn test_tool_result_routing_serde_roundtrip() {
        let mut routing = ToolResultRouting {
            default_region: "custom_region".to_string(),
            keep_results: false,
            max_result_tokens: Some(4096),
            ..Default::default()
        };
        routing
            .tool_overrides
            .insert("read_file".to_string(), "file_reads".to_string());

        let json = serde_json::to_string(&routing).unwrap();
        let back: ToolResultRouting = serde_json::from_str(&json).unwrap();

        assert_eq!(back.default_region, "custom_region");
        assert!(!back.keep_results);
        assert_eq!(back.max_result_tokens, Some(4096));
        assert_eq!(
            back.tool_overrides.get("read_file").map(String::as_str),
            Some("file_reads")
        );
    }

    #[test]
    fn test_stage_with_tool_result_routing_serde_roundtrip() {
        let stages = vec![{
            let mut s = Stage::new("plan".to_string(), make_model());
            s.tool_result_routing = Some(ToolResultRouting {
                default_region: "results".to_string(),
                tool_overrides: HashMap::new(),
                keep_results: true,
                max_result_tokens: Some(2048),
                tool_max_result_tokens: HashMap::new(),
            });
            s
        }];
        let bp = Blueprint::new("t".into(), "d".into(), stages, make_layout());
        let json = serde_json::to_string(&bp).unwrap();
        let back: Blueprint = serde_json::from_str(&json).unwrap();

        let routing = back.stages[0]
            .tool_result_routing
            .as_ref()
            .expect("tool_result_routing should be Some");
        assert_eq!(routing.default_region, "results");
        assert!(routing.keep_results);
        assert_eq!(routing.max_result_tokens, Some(2048));
        assert!(routing.tool_overrides.is_empty());
    }

    // ─── fan_out (StageMode::FanOut) ─────────────────────────────────────────

    fn fanout_config() -> FanOutConfig {
        FanOutConfig {
            worker_agent: None,
            worker_stage: Some("fix_worker".to_string()),
            worker_query: None,
            merge_stage: Some("merge".to_string()),
            max_workers: 3,
            on_worker_failure: WorkerFailurePolicy::Continue,
            split_prompt: "split".to_string(),
            results_region: None,
            max_items: None,
            max_attempts: None,
        }
    }

    #[test]
    fn fanout_stagemode_partial_eq_and_default_policy() {
        let a = StageMode::FanOut {
            config: fanout_config(),
        };
        let b = StageMode::FanOut {
            config: fanout_config(),
        };
        assert_eq!(a, b);
        let mut other = fanout_config();
        other.max_workers = 99;
        assert_ne!(a, StageMode::FanOut { config: other });
        assert_ne!(a, StageMode::Autonomous);
        assert_eq!(
            WorkerFailurePolicy::default(),
            WorkerFailurePolicy::Continue
        );
    }
}
