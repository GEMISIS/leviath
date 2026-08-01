//! Agent blueprints and stage definitions.
//!
//! A blueprint is the complete definition of an agent type, including its
//! execution stages, model selection, tool access, and context layout.
//! Blueprints are typically defined in `leviath.toml` files and can be
//! shared, installed, and versioned.

use crate::error::ValidationError;
use crate::layout::ContextLayout;
use crate::lifecycle::CompactionConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    pub security: Option<crate::taint::SecurityConfig>,

    /// Agent-level override for the batch-tool-calls system-prompt hint. `None`
    /// inherits the global config toggle; a per-stage `batch_tool_hint` overrides
    /// this. See [`crate::taint::resolve_batch_tool_hint`] for the cascade.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_tool_hint: Option<bool>,

    /// Repetition detection configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repetition_detection: Option<RepetitionDetectionConfig>,

    /// File tracking configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_tracking: Option<FileTrackingConfig>,

    /// Agent-level sandbox configuration for tool execution. Per-stage
    /// `[stages.<name>.sandbox]` overrides this; both cascade through
    /// [`crate::resolve_sandbox`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<crate::sandbox::ToolSandboxConfig>,

    /// Opt-in escape hatch: when `true`, the agent may add tools to
    /// its own `tools/` directory mid-run and have them re-discovered and
    /// re-advertised for its next turn. **Off by default** - tools are otherwise
    /// discovered once at spawn and an agent cannot grow its own toolchain.
    #[serde(default)]
    pub dynamic_tools: bool,

    /// Read paths this agent *declares* beyond its workdir - directories a
    /// planner-style agent needs to see, like run archives or design docs.
    /// Declaring is not granting: entries only take effect when the user's
    /// config also grants them (`[security] read_paths`,
    /// `[agent_read_paths.<name>]`, or `allow_blueprint_read_paths = true`),
    /// so an installed manifest cannot widen its own sandbox. Read-only in
    /// every case; `write_file` and `edit_file` stay confined to the workdir.
    /// Semantics live in [`crate::read_paths`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_paths: Option<ReadPathsConfig>,
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
            repetition_detection: None,
            file_tracking: None,
            sandbox: None,
            dynamic_tools: false,
            read_paths: None,
        }
    }

    /// Agent-level tool permissions, keyed by tool name.
    ///
    /// The manifest parser records a top-level `[tool_permissions]` block as
    /// `tool_perm:<tool>` → policy-string entries in [`Self::metadata`]. This
    /// projects them back into a tool-keyed map for the runtime's agent-level
    /// permission layer. Non-`tool_perm:` keys and non-string values are ignored.
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

    /// Add context transforms to this blueprint.
    pub fn with_transforms(mut self, transforms: Vec<ContextTransform>) -> Self {
        self.transforms = transforms;
        self
    }

    /// Set the version of this blueprint.
    pub fn with_version(mut self, version: String) -> Self {
        self.version = version;
        self
    }

    /// Validate that the blueprint is well-formed.
    pub fn validate(&self) -> std::result::Result<(), ValidationError> {
        // Validate context layout
        self.context_layout.validate()?;

        // Check that all stages have valid configurations
        for stage in &self.stages {
            stage.validate()?;
        }

        // Validate transforms reference real regions
        for transform in &self.transforms {
            transform.validate(&self.context_layout)?;
        }

        // Graph validation
        self.validate_graph()?;

        Ok(())
    }

    /// Validate stage graph constraints.
    fn validate_graph(&self) -> std::result::Result<(), ValidationError> {
        let stage_names: std::collections::HashSet<&str> =
            self.stages.iter().map(|s| s.name.as_str()).collect();

        // Entry stage must exist if set
        if let Some(entry) = &self.entry_stage
            && !stage_names.contains(entry.as_str())
        {
            return Err(ValidationError::Graph(format!(
                "entry_stage '{}' does not match any defined stage",
                entry
            )));
        }

        // Fan-out stages reference a worker source + optional merge stage. These
        // are checked even for otherwise-linear blueprints (before the early
        // return below), since `worker_stage`/`merge_stage` name local stages.
        // `worker_agent`/`worker_query` are environment-dependent (resolved
        // against installed agents at run time), so they are not checked here.
        for stage in &self.stages {
            if let StageMode::FanOut { config } = &stage.mode {
                let sources = [
                    config.worker_agent.is_some(),
                    config.worker_stage.is_some(),
                    config.worker_query.is_some(),
                ]
                .iter()
                .filter(|&&set| set)
                .count();
                if sources != 1 {
                    return Err(ValidationError::Stage {
                        stage: stage.name.clone(),
                        message: "fan_out stage must set exactly one of worker_agent, \
                                  worker_stage, or worker_query"
                            .to_string(),
                    });
                }
                if let Some(ws) = &config.worker_stage {
                    match self.stages.iter().find(|s| &s.name == ws) {
                        None => {
                            return Err(ValidationError::Stage {
                                stage: stage.name.clone(),
                                message: format!("fan_out worker_stage '{}' does not exist", ws),
                            });
                        }
                        Some(target) if !target.allow_as_worker => {
                            return Err(ValidationError::Stage {
                                stage: stage.name.clone(),
                                message: format!(
                                    "fan_out worker_stage '{}' must set allow_as_worker = true",
                                    ws
                                ),
                            });
                        }
                        Some(_) => {}
                    }
                }
                if let Some(ms) = &config.merge_stage
                    && !stage_names.contains(ms.as_str())
                {
                    return Err(ValidationError::Stage {
                        stage: stage.name.clone(),
                        message: format!("fan_out merge_stage '{}' does not exist", ms),
                    });
                }
            }
        }

        let has_any_transitions = self.stages.iter().any(|s| s.transitions.is_some());
        if !has_any_transitions {
            // Pure linear mode - no graph validation needed
            return Ok(());
        }

        // All transition targets must exist
        for stage in &self.stages {
            if let Some(ref transitions) = stage.transitions {
                for (target_name, edge) in transitions {
                    if !stage_names.contains(target_name.as_str()) {
                        return Err(ValidationError::Transition {
                            from: stage.name.clone(),
                            to: target_name.clone(),
                            message: "target stage does not exist".to_string(),
                        });
                    }
                    // A `stuck` edge with no threshold could never fire. Caught
                    // here as well as in the manifest parser, so blueprints built
                    // programmatically (API / `lev validate`) are held to it too.
                    if edge.condition == TransitionCondition::Stuck
                        && !edge.stuck.is_some_and(|c| c.is_armed())
                    {
                        return Err(ValidationError::Transition {
                            from: stage.name.clone(),
                            to: target_name.clone(),
                            message: "condition = \"stuck\" requires at least one \
                                      stuck_after_* threshold (the edge could never fire)"
                                .to_string(),
                        });
                    }
                }

                // A `require_modifications` gate on a stage that advertises no
                // file-modifying tool can never be satisfied - it would just
                // burn the stage's re-run budget every time.
                for (target_name, edge) in transitions {
                    let Some(gate) = &edge.gate else { continue };
                    if !gate.require_modifications {
                        continue;
                    }
                    let can_modify = stage.available_tools.iter().any(|t| {
                        MODIFYING_TOOLS.contains(&t.as_str())
                            || gate.tools.iter().any(|extra| extra == t)
                    });
                    if !can_modify {
                        return Err(ValidationError::Transition {
                            from: stage.name.clone(),
                            to: target_name.clone(),
                            message: "gate requires modifications, but the stage has no \
                                      file-modifying tool in available_tools"
                                .to_string(),
                        });
                    }
                }

                // Self-loop safety: stages that transition to themselves need max_revisits
                if transitions.contains_key(&stage.name) && stage.max_revisits.is_none() {
                    return Err(ValidationError::Stage {
                        stage: stage.name.clone(),
                        message: "self-loop transition requires max_revisits".to_string(),
                    });
                }
            }
        }

        // At least one terminal path must exist (a stage with no outgoing transitions,
        // or with only conditional transitions that may not fire)
        let entry = self.resolve_entry_stage_name();
        let has_terminal = self.has_terminal_path(&entry, &mut std::collections::HashSet::new());
        if !has_terminal {
            return Err(ValidationError::Graph(
                "no terminal path exists from entry stage - agent would never complete".to_string(),
            ));
        }

        Ok(())
    }

    /// Resolve the entry stage name.
    pub fn resolve_entry_stage_name(&self) -> String {
        self.entry_stage.clone().unwrap_or_else(|| {
            self.stages
                .first()
                .map(|s| s.name.clone())
                .unwrap_or_default()
        })
    }

    /// Check if there is a terminal path reachable from `stage_name`.
    fn has_terminal_path(
        &self,
        stage_name: &str,
        visited: &mut std::collections::HashSet<String>,
    ) -> bool {
        if visited.contains(stage_name) {
            return false;
        }
        visited.insert(stage_name.to_string());

        let stage = self.stages.iter().find(|s| s.name == stage_name);
        let stage = match stage {
            Some(s) => s,
            // Unreachable via this function's only call site (`validate_graph`,
            // below): it rejects any transition target that doesn't match a
            // real stage name *before* ever calling `has_terminal_path`, and
            // `has_terminal_path` is private, so no other caller can pass in
            // an unvalidated stage name.
            None => return false,
        };

        // A fan-out stage with a merge stage hands off to it after workers
        // complete, so its terminal path runs through the merge stage.
        if let StageMode::FanOut {
            config:
                FanOutConfig {
                    merge_stage: Some(ms),
                    ..
                },
        } = &stage.mode
        {
            return self.has_terminal_path(ms, visited);
        }

        match &stage.transitions {
            None => {
                // Linear mode: check if there's a next stage by index
                let idx = self
                    .stages
                    .iter()
                    .position(|s| s.name == stage_name)
                    .unwrap_or(0);
                if idx + 1 >= self.stages.len() {
                    return true; // terminal
                }
                self.has_terminal_path(&self.stages[idx + 1].name, visited)
            }
            Some(transitions) => {
                if transitions.is_empty() {
                    return true; // terminal stage
                }
                // Check if any transition leads to a terminal
                for target in transitions.keys() {
                    if self.has_terminal_path(target, visited) {
                        return true;
                    }
                }
                // If all targets are exhaustible (already visited + have max_revisits),
                // the stage will eventually have zero available edges → terminal
                transitions.keys().all(|target| {
                    self.stages
                        .iter()
                        .find(|s| s.name == *target)
                        .map(|s| s.max_revisits.is_some())
                        .unwrap_or(false)
                })
            }
        }
    }

    /// Find a stage by name.
    pub fn find_stage(&self, name: &str) -> Option<&Stage> {
        self.stages.iter().find(|s| s.name == name)
    }
}

/// Configuration for automatic file tracking in context regions.
///
/// When configured, read_file/write_file results are automatically synced to a
/// HashMap region, and tool results reference the system prompt instead of
/// duplicating content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTrackingConfig {
    /// Name of the HashMap region to sync files to
    pub region: String,
    /// Auto-update on read_file
    #[serde(default = "default_true_val")]
    pub track_reads: bool,
    /// Auto-update on write_file. (`edit_file` is not tracked: its arguments are
    /// `old_str`/`new_str`, so the post-edit file body isn't available without a
    /// re-read.)
    #[serde(default = "default_true_val")]
    pub track_writes: bool,
    /// Truncate files larger than this token count in context
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_tokens: Option<usize>,
}

fn default_true_val() -> bool {
    true
}

/// Configuration for repetition detection in the inference loop.
///
/// Controls thresholds for detecting degenerate read loops where agents
/// call the same tool repeatedly without productive action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepetitionDetectionConfig {
    /// Maximum times the same tool+args combo may repeat before a nudge.
    pub max_repeat_calls: Option<usize>,
    /// Maximum consecutive read-only calls with no productive calls in between.
    pub max_readonly_streak: Option<usize>,
    /// Whether detection is enabled. Default: true.
    pub enabled: Option<bool>,
}

/// Configuration for routing tool results to specific context window regions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultRouting {
    /// Default region for tool results (default: "tool_results")
    pub default_region: String,
    /// Per-tool overrides: tool_name → region_name
    pub tool_overrides: HashMap<String, String>,
    /// Whether to keep tool results (true) or discard after use (false)
    pub persist: bool,
    /// Max tokens per tool result (truncate if larger)
    pub max_result_tokens: Option<usize>,
}

impl Default for ToolResultRouting {
    fn default() -> Self {
        Self {
            default_region: "tool_results".to_string(),
            tool_overrides: HashMap::new(),
            persist: true,
            max_result_tokens: None,
        }
    }
}

/// Interaction mode for a stage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum StageMode {
    /// Runs without user input, fully autonomous
    #[default]
    Autonomous,

    /// Requires user input before starting
    Interactive,

    /// Can receive input at defined points during execution
    InteractivePoints {
        /// Points where user input can be requested
        points: Vec<InteractionPoint>,
    },

    /// Splits work into JSON items and runs them across parallel in-process
    /// sub-agent workers, then optionally merges before transitioning.
    FanOut {
        /// Fan-out configuration (worker source, concurrency, failure policy).
        config: FanOutConfig,
    },
}

impl PartialEq for StageMode {
    #[inline(never)]
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Autonomous, Self::Autonomous) | (Self::Interactive, Self::Interactive) => true,
            (Self::InteractivePoints { points: a }, Self::InteractivePoints { points: b }) => {
                a == b
            }
            (Self::FanOut { config: a }, Self::FanOut { config: b }) => a == b,
            _ => false,
        }
    }
}
impl Eq for StageMode {}

/// How a fan-out stage handles worker failures.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFailurePolicy {
    /// Run the merge/next stage with the successful workers; failures are
    /// reported into the consolidated results.
    #[default]
    Continue,
    /// Any worker failure routes the fan-out stage down its `error` edge.
    FailAll,
}

/// Configuration for a [`StageMode::FanOut`] stage.
///
/// Exactly one of `worker_agent` / `worker_stage` / `worker_query` selects the
/// worker's agent type (validated when the blueprint's graph is checked).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FanOutConfig {
    /// A separate registered/installed blueprint run as the worker agent type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_agent: Option<String>,
    /// A stage in *this* blueprint (self-as-agent-type); must be marked
    /// `allow_as_worker = true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_stage: Option<String>,
    /// Discovery hint matched against installed agent types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_query: Option<String>,
    /// Optional stage that reconciles worker results before transitioning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_stage: Option<String>,
    /// Maximum number of workers running concurrently.
    #[serde(default = "default_max_workers")]
    pub max_workers: usize,
    /// How to handle worker failures.
    #[serde(default)]
    pub on_worker_failure: WorkerFailurePolicy,
    /// Prompt that produces the JSON array of work items (one per worker).
    #[serde(default)]
    pub split_prompt: String,
}

/// Default `max_workers` when unspecified.
fn default_max_workers() -> usize {
    4
}

/// Style of interaction at an interaction point.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionStyle {
    /// Free-form text answer (default).
    #[default]
    FreeText,
    /// User picks one option from a list.
    MultipleChoice,
    /// Simple yes/no confirmation.
    Confirm,
}

impl PartialEq for InteractionStyle {
    #[inline(never)]
    fn eq(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}
impl Eq for InteractionStyle {}

/// A point where a stage can request user input.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InteractionPoint {
    /// Unique name for this interaction point
    pub name: String,

    /// Prompt to show the user
    pub prompt: String,

    /// Whether input is required (vs optional)
    pub required: bool,

    /// Style of interaction (free text, multiple choice, confirm)
    #[serde(default)]
    pub style: InteractionStyle,

    /// Options for MultipleChoice style
    #[serde(default)]
    pub options: Vec<String>,

    /// Directives keyed by option label.
    ///
    /// When the user picks an option present in this map (e.g. "Revise - I'll
    /// describe changes"), the mapped directive text is injected into the
    /// agent's conversation context and the stage re-runs inference IN-STAGE
    /// (bounded by a revision cap) instead of falling through to a stage
    /// transition. The directive tells the agent what to do next - e.g. call
    /// `ask_user_text` to learn what to change, or `edit_document` to let the
    /// user edit the plan directly - so the routing decision is deterministic
    /// (code) while the actual input capture is an agent tool call.
    #[serde(default, alias = "followups")]
    pub directives: HashMap<String, String>,

    /// Options that, when selected, immediately abort the run: the engine
    /// marks the run cancelled and stops with no further inference and no
    /// transition resolution. Matched against the selected option label with
    /// the same dash/whitespace normalization used for directive lookup.
    #[serde(default)]
    pub abort_options: Vec<String>,

    /// Options that, when selected, open the stage's most recent output (e.g.
    /// the plan) in an editable field so the user can modify it directly. The
    /// engine issues the edit interaction itself and injects the edited text
    /// back into context - deterministic, with no dependence on the model
    /// choosing to call an edit tool. Matched with the same normalization.
    #[serde(default)]
    pub edit_options: Vec<String>,

    /// Optional pinned region to hold this point's authoritative document (e.g.
    /// `"plan"`). When set, each time the point is presented the current
    /// document - the produced text, or the user's direct edit - *replaces* that
    /// region's content, so later revisions and downstream stages build on the
    /// current version rather than regenerating from the task. `None` ⇒ the
    /// document lives only in the rolling conversation / output.
    #[serde(default)]
    pub document_region: Option<String>,
}

/// A single execution stage in an agent's workflow.
///
/// Stages allow an agent to use different models or configurations for
/// different phases of work. For example, a coding agent might have:
/// - Analyze stage: fast model for understanding requirements
/// - Implement stage: powerful model for code generation
/// - Review stage: critique model for checking quality
///
/// Each stage can have its own context layout (memory structure), allowing
/// different stages to have different region configurations optimized for
/// their specific needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stage {
    /// Name of this stage
    pub name: String,

    /// Description of what this stage does
    pub description: Option<String>,

    /// Model to use for this stage
    pub model: ModelConfig,

    /// Which tools are available in this stage
    pub available_tools: Vec<String>,

    /// Maximum iterations for this stage
    pub max_iterations: Option<usize>,

    /// Interaction mode (autonomous or interactive)
    #[serde(default)]
    pub mode: StageMode,

    /// Optional stage-specific context layout
    /// If None, uses the blueprint's global layout
    pub context_layout: Option<ContextLayout>,

    /// Custom configuration for this stage
    pub config: HashMap<String, serde_json::Value>,

    /// Per-tool permission overrides for this stage.
    /// Keys: tool name. Values: "allow" | "ask" | "deny".
    /// Narrower than agent-level, wider than launch flags.
    #[serde(default)]
    pub tool_permissions: HashMap<String, String>,

    /// If true, don't advance to the next stage until all children spawned
    /// during this stage have completed.
    #[serde(default)]
    pub requires_children: bool,

    /// Directed transitions from this stage (None = linear/next-in-list)
    pub transitions: Option<HashMap<String, TransitionEdge>>,

    /// Max times this stage can be re-entered (revisits, not counting first visit)
    pub max_revisits: Option<usize>,

    /// Custom prompt for transition decisions (overrides default)
    pub transition_prompt: Option<String>,

    /// Whether this stage accepts mid-run user messages.
    /// When true, messages sent to the agent are injected into context
    /// between inference calls. Default: true.
    #[serde(default = "default_true")]
    pub accepts_messages: bool,

    /// Whether the LLM may end the run at this stage instead of naming a
    /// transition target - e.g. a review stage that approves the work
    /// needs no further stage. When true, `prompt_llm_transition`'s query
    /// offers an explicit "DONE" response that resolves to a terminal
    /// (no-transition) outcome instead of forcing the single/first
    /// available edge.
    #[serde(default)]
    pub allow_complete: bool,

    /// Whether this stage may be used as a fan-out `worker_stage` - i.e. run as
    /// an in-process sub-agent worker entered at this stage. Off by default so a
    /// blueprint author must explicitly opt a stage in to being fanned into
    /// (you can only fan out into a stage designed for it).
    #[serde(default)]
    pub allow_as_worker: bool,

    /// Per-stage taint/security override. `None` inherits the agent-level
    /// `Blueprint.security` (which in turn inherits the global config toggle).
    /// Set `taint_tracking = false` here to opt a single stage out, or `true`
    /// to opt it in independently of the agent/global setting.
    #[serde(default)]
    pub security: Option<crate::taint::SecurityConfig>,

    /// Per-stage override for the batch-tool-calls system-prompt hint. `None`
    /// inherits the agent-level `Blueprint.batch_tool_hint` (which in turn
    /// inherits the global config toggle). Set `false` to opt a sequential stage
    /// out, or `true` to opt it in independently of the agent/global setting.
    #[serde(default)]
    pub batch_tool_hint: Option<bool>,

    /// Per-stage sandbox override. `None` inherits the agent-level
    /// `Blueprint.sandbox` (which in turn inherits the global default = host).
    /// Set a tighter sandbox here to isolate a single stage - e.g. run analysis
    /// on the host but implementation in a networkless container.
    #[serde(default)]
    pub sandbox: Option<crate::sandbox::ToolSandboxConfig>,

    /// Optional routing configuration for tool results.
    /// When set, tool results are routed to the configured region(s) instead
    /// of the default "conversation" region.
    #[serde(default)]
    pub tool_result_routing: Option<ToolResultRouting>,
}

/// Default value for bool fields that should default to true.
fn default_true() -> bool {
    true
}

impl Stage {
    /// Create a new stage with the specified configuration.
    pub fn new(name: String, model: ModelConfig) -> Self {
        Self {
            name,
            description: None,
            model,
            available_tools: Vec::new(),
            max_iterations: None,
            mode: StageMode::Autonomous,
            context_layout: None,
            config: HashMap::new(),
            tool_permissions: HashMap::new(),
            requires_children: false,
            transitions: None,
            max_revisits: None,
            transition_prompt: None,
            accepts_messages: true,
            allow_complete: false,
            allow_as_worker: false,
            security: None,
            batch_tool_hint: None,
            sandbox: None,
            tool_result_routing: None,
        }
    }

    /// Add tools to this stage.
    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.available_tools = tools;
        self
    }

    /// Set the interaction mode for this stage.
    pub fn with_mode(mut self, mode: StageMode) -> Self {
        self.mode = mode;
        self
    }

    /// Set a stage-specific context layout.
    pub fn with_context_layout(mut self, layout: ContextLayout) -> Self {
        self.context_layout = Some(layout);
        self
    }

    /// Set the description for this stage.
    pub fn with_description(mut self, description: String) -> Self {
        self.description = Some(description);
        self
    }

    /// Validate that this stage is well-formed.
    fn validate(&self) -> std::result::Result<(), ValidationError> {
        if self.name.is_empty() {
            return Err(ValidationError::Stage {
                stage: "(empty)".to_string(),
                message: "stage name cannot be empty".to_string(),
            });
        }

        // Validate stage-specific context layout if present
        if let Some(layout) = &self.context_layout {
            layout.validate()?;
        }

        Ok(())
    }
}

/// A single model entry within a [`ModelConfig`] models list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelEntry {
    /// Provider name (e.g., "anthropic", "openai")
    pub provider: String,

    /// Model identifier (e.g., "claude-sonnet-4-6")
    pub model: String,
}

impl ModelEntry {
    pub fn new(provider: String, model: String) -> Self {
        Self { provider, model }
    }
}

/// Model configuration for a stage.
///
/// Models are specified as an ordered priority list in `models`. The first
/// entry whose provider is registered at runtime is used. When
/// `allow_user_default` is true (the default), the user's configured default
/// model is tried as a last resort. When false, the stage fails if none of
/// the listed models are available.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Ordered list of models to try (first available wins).
    #[serde(default)]
    pub models: Vec<ModelEntry>,

    /// When true (default), fall back to the user's configured default model
    /// if none of the listed models are available.
    #[serde(default = "default_allow_user_default")]
    pub allow_user_default: bool,

    /// Optional parameters that apply to whichever model gets selected.
    #[serde(default)]
    pub parameters: HashMap<String, serde_json::Value>,

    /// Optional per-stage cap on the wall-clock time (in seconds) one inference
    /// for this stage may run - the whole call including retries. When set, it
    /// overrides the default job timeout; when `None`, the default applies.
    ///
    /// This lets a stage with slow first-token latency (e.g. a large-prompt
    /// analyze call) get a long cap while a quick iterative stage fails fast on
    /// a stalled connection instead of hanging for the full default.
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,
}

fn default_allow_user_default() -> bool {
    true
}

impl ModelConfig {
    /// Create a new model configuration with a single model entry.
    pub fn new(provider: String, model: String) -> Self {
        Self {
            models: vec![ModelEntry::new(provider, model)],
            allow_user_default: true,
            parameters: HashMap::new(),
            request_timeout_secs: None,
        }
    }

    /// Convenience: provider of the first model entry (for backward compat).
    pub fn provider(&self) -> &str {
        self.models
            .first()
            .map(|e| e.provider.as_str())
            .unwrap_or("anthropic")
    }

    /// Convenience: model name of the first model entry (for backward compat).
    pub fn model(&self) -> &str {
        self.models
            .first()
            .map(|e| e.model.as_str())
            .unwrap_or("claude-sonnet-4-6")
    }
}

/// Context transform for converting between agent types.
///
/// When spawning a sub-agent with a different blueprint, transforms define
/// how to map regions from the parent agent's context to the child agent's
/// context. This enables smooth handoffs between agents with different
/// memory structures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextTransform {
    /// Source blueprint name
    pub from_blueprint: String,

    /// Target blueprint name
    pub to_blueprint: String,

    /// Region mapping rules
    pub mappings: Vec<RegionMapping>,
}

impl ContextTransform {
    /// Validate that this transform references valid regions.
    fn validate(&self, layout: &ContextLayout) -> std::result::Result<(), ValidationError> {
        for mapping in &self.mappings {
            // We can only validate target regions against the current layout
            // (source regions belong to a different blueprint)
            if layout.get_region(&mapping.to_region).is_none() {
                return Err(ValidationError::Region {
                    region: mapping.to_region.clone(),
                    message: "transform target region not found in layout".to_string(),
                });
            }
        }
        Ok(())
    }
}

/// Mapping rule for a single region in a context transform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionMapping {
    /// Source region name
    pub from_region: String,

    /// Target region name
    pub to_region: String,

    /// Optional transformation to apply to content
    pub transform: Option<ContentTransform>,
}

/// A directed transition edge from one stage to another.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionEdge {
    /// Target stage name (derived from the HashMap key during parsing)
    pub target: String,

    /// When this edge is available
    #[serde(default)]
    pub condition: TransitionCondition,

    /// Human-readable hint for the LLM
    pub hint: Option<String>,

    /// How context transforms when crossing this edge
    #[serde(default)]
    pub transform: EdgeTransform,

    /// Preconditions the agent must satisfy before this edge may be followed.
    /// Absent ⇒ the edge is unconditional (beyond its `condition`).
    #[serde(default)]
    pub gate: Option<TransitionGate>,

    /// Thresholds arming a [`TransitionCondition::Stuck`] edge. `Some` iff the
    /// condition is `Stuck` - both the manifest parser and [`Blueprint::validate`]
    /// reject the two half-configured shapes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stuck: Option<StuckConfig>,
}

/// Thresholds that arm a [`TransitionCondition::Stuck`] edge.
///
/// At least one threshold is always set: an edge with none could never fire, so
/// both the manifest parser and [`Blueprint::validate`] reject that shape rather
/// than build a dead edge. Every threshold is evaluated against the *current
/// stage's* progress counters, which reset on each stage entry - so a blueprint
/// can arm different stages with different thresholds independently.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StuckConfig {
    /// `stuck_after_iterations`: inferences run in this stage without finishing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_iterations: Option<usize>,

    /// `stuck_after_minutes`: wall-clock minutes spent in this stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_minutes: Option<usize>,

    /// `stuck_after_same_file_edits`: `write_file`/`edit_file` calls against a
    /// single path in this stage - the "100 iterations in the wrong file" mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_same_file_edits: Option<usize>,

    /// `stuck_after_tool_calls`: total tool calls made in this stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_tool_calls: Option<usize>,
}

impl StuckConfig {
    /// Whether any threshold is set. `false` ⇒ the edge could never fire.
    pub fn is_armed(&self) -> bool {
        self.after_iterations.is_some()
            || self.after_minutes.is_some()
            || self.after_same_file_edits.is_some()
            || self.after_tool_calls.is_some()
    }
}

/// Preconditions an edge imposes on the stage it leaves, checked once the edge
/// has been chosen but before its transform runs. A gate that isn't satisfied
/// re-runs the stage with a `[System]` nudge instead of transitioning.
///
/// The motivating case: an agent that reads and reasons about the
/// codebase entirely through `shell` and reaches the review stage without ever
/// having called a file-writing tool, producing a run with no output at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionGate {
    /// Require at least one successful file-modifying tool call in the stage
    /// being left.
    #[serde(default)]
    pub require_modifications: bool,

    /// Nudge injected when the gate blocks. A default explaining the framework's
    /// change tracking is generated when absent.
    #[serde(default)]
    pub message: Option<String>,

    /// Region whose non-emptiness also satisfies the gate. Per-stage tool-call
    /// counters reset on stage entry and are not restored when a run resumes
    /// after a daemon restart, but context regions are - so pointing the gate at
    /// the region the write tools are routed into keeps a resumed run honest.
    #[serde(default)]
    pub region: Option<String>,

    /// Tool names counted as modifying beyond the built-in `write_file` /
    /// `edit_file` - for agents whose writes go through MCP or script tools.
    #[serde(default)]
    pub tools: Vec<String>,

    /// How many times the stage is re-run before the gate gives up and lets the
    /// transition through (with a warning). Defaults to
    /// [`DEFAULT_GATE_ATTEMPTS`].
    #[serde(default)]
    pub max_attempts: Option<usize>,
}

/// Default re-run budget for an unsatisfied [`TransitionGate`].
pub const DEFAULT_GATE_ATTEMPTS: usize = 3;

/// Built-in tools that modify files on disk, for [`TransitionGate`]'s
/// `require_modifications` accounting. Extended per-edge by
/// [`TransitionGate::tools`].
pub const MODIFYING_TOOLS: &[&str] = &["write_file", "edit_file"];

/// Condition that determines when a transition edge is available.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionCondition {
    /// Always available (LLM chooses)
    #[default]
    Always,
    /// Only on error
    Error,
    /// Only when max_iterations hit
    MaxIterations,
    /// LLM picks from available transitions (default for multi-transition stages)
    LlmChoice,
    /// Fires *mid-stage* when the stage's runtime metrics cross this edge's
    /// [`StuckConfig`] thresholds - the agent is burning iterations, wall clock,
    /// or edits to one file without finishing. Unlike every other condition this
    /// interrupts a stage the agent never said it had completed, so when the edge
    /// is unavailable the runtime resumes the stage rather than transitioning.
    Stuck,
}

/// How context transforms when crossing a transition edge.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeTransform {
    /// Copy everything as-is (default for single-transition linear stages)
    #[default]
    Direct,

    /// Clear stage-specific regions, keep pinned/system
    Clear,

    /// LLM-compact stage content into summary
    Compact {
        #[serde(default)]
        prompt: Option<String>,
    },

    /// Per-region rules
    Custom {
        carry: Vec<String>,
        compact: Vec<String>,
        clear: Vec<String>,
        compact_prompt: Option<String>,
    },
}

impl PartialEq for EdgeTransform {
    #[inline(never)]
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Direct, Self::Direct) | (Self::Clear, Self::Clear) => true,
            (Self::Compact { prompt: a }, Self::Compact { prompt: b }) => a == b,
            (
                Self::Custom {
                    carry: ca,
                    compact: coa,
                    clear: cla,
                    compact_prompt: cpa,
                },
                Self::Custom {
                    carry: cb,
                    compact: cob,
                    clear: clb,
                    compact_prompt: cpb,
                },
            ) => ca == cb && coa == cob && cla == clb && cpa == cpb,
            _ => false,
        }
    }
}
impl Eq for EdgeTransform {}

/// Content transformation type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentTransform {
    /// Copy content as-is
    Direct,

    /// Summarize content to fit target region
    Summarize,

    /// Extract specific fields
    Extract { fields: Vec<String> },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::ContextLayout;
    use crate::layout::RegionDefinition;
    use crate::region::RegionKind;

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
    fn test_blueprint_with_transforms_version() {
        let stages = vec![Stage::new("plan".to_string(), make_model())];
        let bp = Blueprint::new("t".into(), "d".into(), stages, make_layout())
            .with_transforms(vec![ContextTransform {
                from_blueprint: "a".to_string(),
                to_blueprint: "b".to_string(),
                mappings: vec![],
            }])
            .with_version("2.0.0".to_string());

        assert_eq!(bp.transforms.len(), 1);
        assert_eq!(bp.version, "2.0.0");
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
    fn test_blueprint_validate_runs_transform_validation() {
        // A transform whose mapping targets a real region - validate() must
        // reach ContextTransform::validate() and succeed.
        let stages = vec![Stage::new("plan".to_string(), make_model())];
        let mut bp = Blueprint::new("t".into(), "d".into(), stages, make_layout());
        bp.transforms.push(ContextTransform {
            from_blueprint: "a".to_string(),
            to_blueprint: "b".to_string(),
            mappings: vec![RegionMapping {
                from_region: "test".to_string(),
                to_region: "test".to_string(),
                transform: None,
            }],
        });
        assert!(bp.validate().is_ok());
    }

    #[test]
    fn test_blueprint_validate_fails_on_transform_targeting_unknown_region() {
        let stages = vec![Stage::new("plan".to_string(), make_model())];
        let mut bp = Blueprint::new("t".into(), "d".into(), stages, make_layout());
        bp.transforms.push(ContextTransform {
            from_blueprint: "a".to_string(),
            to_blueprint: "b".to_string(),
            mappings: vec![RegionMapping {
                from_region: "test".to_string(),
                to_region: "nonexistent".to_string(),
                transform: None,
            }],
        });
        let err = bp.validate().unwrap_err();
        assert_eq!(
            err,
            ValidationError::Region {
                region: "nonexistent".to_string(),
                message: "transform target region not found in layout".to_string(),
            }
        );
    }

    #[test]
    fn test_mixed_linear_and_graph_mode_terminal_path() {
        // "plan" has explicit transitions (triggers graph-mode validation),
        // but "impl" and "review" have none - they must fall back to
        // linear (next-by-index) terminal-path resolution.
        let mut plan = Stage::new("plan".to_string(), make_model());
        let impl_stage = Stage::new("impl".to_string(), make_model());
        let review = Stage::new("review".to_string(), make_model());

        let mut transitions = HashMap::new();
        transitions.insert(
            "impl".to_string(),
            TransitionEdge {
                target: "impl".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
                gate: None,
                stuck: None,
            },
        );
        plan.transitions = Some(transitions);

        let bp = Blueprint::new(
            "t".into(),
            "".into(),
            vec![plan, impl_stage, review],
            make_layout(),
        );
        assert!(bp.validate().is_ok());
    }

    #[test]
    fn test_stage_validation() {
        let stage = Stage::new(
            "test".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
        );
        assert!(stage.validate().is_ok());

        let empty_stage = Stage::new(
            "".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
        );
        assert!(empty_stage.validate().is_err());
    }

    #[test]
    fn test_stage_validate_with_valid_context_layout_is_ok() {
        let mut stage = Stage::new("test".to_string(), make_model());
        stage.context_layout = Some(make_layout());
        assert!(stage.validate().is_ok());
    }

    #[test]
    fn test_stage_validate_with_invalid_context_layout_is_err() {
        // Duplicate region names make the layout itself invalid.
        let regions = vec![
            RegionDefinition::new("dup".to_string(), RegionKind::Pinned, 100),
            RegionDefinition::new("dup".to_string(), RegionKind::Temporary, 100),
        ];
        let mut stage = Stage::new("test".to_string(), make_model());
        stage.context_layout = Some(ContextLayout::new(regions, 200));
        assert!(stage.validate().is_err());
    }

    #[test]
    fn test_stage_with_tools_context_layout_description() {
        let stage = Stage::new("test".to_string(), make_model())
            .with_tools(vec!["read_file".to_string(), "bash".to_string()])
            .with_context_layout(make_layout())
            .with_description("does things".to_string());

        assert_eq!(stage.available_tools, vec!["read_file", "bash"]);
        assert!(stage.context_layout.is_some());
        assert_eq!(stage.description.as_deref(), Some("does things"));
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

    #[test]
    fn test_model_config_convenience_accessors() {
        let mc = ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string());
        assert_eq!(mc.provider(), "anthropic");
        assert_eq!(mc.model(), "claude-sonnet-4-6");
    }

    #[test]
    fn test_model_config_convenience_accessors_empty_models() {
        let mc = ModelConfig {
            models: vec![],
            allow_user_default: true,
            parameters: HashMap::new(),
            request_timeout_secs: None,
        };
        assert_eq!(mc.provider(), "anthropic");
        assert_eq!(mc.model(), "claude-sonnet-4-6");
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
    fn test_graph_validation_entry_stage_exists() {
        let stages = vec![Stage::new("plan".to_string(), make_model())];
        let mut bp = Blueprint::new("t".into(), "".into(), stages, make_layout());
        bp.entry_stage = Some("nonexistent".to_string());
        assert!(bp.validate().is_err());
    }

    #[test]
    fn test_graph_validation_entry_stage_valid() {
        let stages = vec![Stage::new("plan".to_string(), make_model())];
        let mut bp = Blueprint::new("t".into(), "".into(), stages, make_layout());
        bp.entry_stage = Some("plan".to_string());
        assert!(bp.validate().is_ok());
    }

    #[test]
    fn test_graph_validation_transition_target_missing() {
        let mut stage = Stage::new("plan".to_string(), make_model());
        let mut transitions = HashMap::new();
        transitions.insert(
            "nonexistent".to_string(),
            TransitionEdge {
                target: "nonexistent".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
                gate: None,
                stuck: None,
            },
        );
        stage.transitions = Some(transitions);
        let bp = Blueprint::new("t".into(), "".into(), vec![stage], make_layout());
        assert!(bp.validate().is_err());
    }

    /// A `require_modifications` gate on a stage that can't modify anything
    /// could never be satisfied - it would just burn the stage's re-run budget
    /// on every pass. Reject it at load time instead.
    #[test]
    fn test_graph_validation_modification_gate_needs_a_writing_stage() {
        let gated = |tools: &[&str], extra: &[&str]| {
            let mut stage = Stage::new("impl".to_string(), make_model());
            stage.available_tools = tools.iter().map(|t| t.to_string()).collect();
            let mut transitions = HashMap::new();
            transitions.insert(
                "review".to_string(),
                TransitionEdge {
                    target: "review".to_string(),
                    condition: TransitionCondition::Always,
                    hint: None,
                    transform: EdgeTransform::Direct,
                    stuck: None,
                    gate: Some(TransitionGate {
                        require_modifications: true,
                        tools: extra.iter().map(|t| t.to_string()).collect(),
                        ..Default::default()
                    }),
                },
            );
            stage.transitions = Some(transitions);
            Blueprint::new(
                "t".into(),
                "".into(),
                vec![stage, Stage::new("review".to_string(), make_model())],
                make_layout(),
            )
        };
        let err = gated(&["read_file"], &[]).validate().unwrap_err();
        assert!(err.to_string().contains("no file-modifying tool"));
        // A built-in write tool satisfies it...
        assert!(gated(&["read_file", "edit_file"], &[]).validate().is_ok());
        // ...as does one the gate itself declares (MCP / script toolchains).
        assert!(
            gated(&["read_file", "patch_file"], &["patch_file"])
                .validate()
                .is_ok()
        );
        // A gate that doesn't require modifications is never checked.
        let mut off = gated(&["read_file"], &[]);
        off.stages[0]
            .transitions
            .as_mut()
            .unwrap()
            .get_mut("review")
            .unwrap()
            .gate = Some(TransitionGate::default());
        assert!(off.validate().is_ok());
        // Neither is an edge with no gate at all.
        off.stages[0]
            .transitions
            .as_mut()
            .unwrap()
            .get_mut("review")
            .unwrap()
            .gate = None;
        assert!(off.validate().is_ok());
    }

    #[test]
    fn test_graph_validation_self_loop_requires_max_revisits() {
        let mut stage = Stage::new("impl".to_string(), make_model());
        let mut transitions = HashMap::new();
        transitions.insert(
            "impl".to_string(),
            TransitionEdge {
                target: "impl".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
                gate: None,
                stuck: None,
            },
        );
        stage.transitions = Some(transitions);
        let bp = Blueprint::new("t".into(), "".into(), vec![stage], make_layout());
        assert!(bp.validate().is_err());
    }

    #[test]
    fn test_graph_validation_self_loop_with_max_revisits_ok() {
        let mut stage = Stage::new("impl".to_string(), make_model());
        stage.max_revisits = Some(3);
        let mut transitions = HashMap::new();
        transitions.insert(
            "impl".to_string(),
            TransitionEdge {
                target: "impl".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
                gate: None,
                stuck: None,
            },
        );
        stage.transitions = Some(transitions);
        let bp = Blueprint::new("t".into(), "".into(), vec![stage], make_layout());
        // Should pass: self-loop has max_revisits, and the self-loop target
        // will eventually exhaust, leaving zero edges → terminal
        assert!(bp.validate().is_ok());
    }

    #[test]
    fn test_graph_validation_terminal_path_exists() {
        let mut plan = Stage::new("plan".to_string(), make_model());
        let mut review = Stage::new("review".to_string(), make_model());
        review.transitions = Some(HashMap::new()); // terminal: no outgoing

        let mut transitions = HashMap::new();
        transitions.insert(
            "review".to_string(),
            TransitionEdge {
                target: "review".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
                gate: None,
                stuck: None,
            },
        );
        plan.transitions = Some(transitions);

        let bp = Blueprint::new("t".into(), "".into(), vec![plan, review], make_layout());
        assert!(bp.validate().is_ok());
    }

    #[test]
    fn test_graph_no_terminal_path() {
        // Two stages that only transition to each other with no terminal
        let mut a = Stage::new("a".to_string(), make_model());
        let mut b = Stage::new("b".to_string(), make_model());

        let mut a_transitions = HashMap::new();
        a_transitions.insert(
            "b".to_string(),
            TransitionEdge {
                target: "b".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
                gate: None,
                stuck: None,
            },
        );
        a.transitions = Some(a_transitions);

        let mut b_transitions = HashMap::new();
        b_transitions.insert(
            "a".to_string(),
            TransitionEdge {
                target: "a".to_string(),
                condition: TransitionCondition::Always,
                hint: None,
                transform: EdgeTransform::Direct,
                gate: None,
                stuck: None,
            },
        );
        b.transitions = Some(b_transitions);

        let bp = Blueprint::new("t".into(), "".into(), vec![a, b], make_layout());
        assert!(bp.validate().is_err());
    }

    #[test]
    fn test_linear_stages_still_validate() {
        // No transitions set at all - pure linear mode
        let stages = vec![
            Stage::new("plan".to_string(), make_model()),
            Stage::new("impl".to_string(), make_model()),
            Stage::new("review".to_string(), make_model()),
        ];
        let bp = Blueprint::new("t".into(), "".into(), stages, make_layout());
        assert!(bp.validate().is_ok());
    }

    #[test]
    fn test_resolve_entry_stage_name() {
        let stages = vec![
            Stage::new("plan".to_string(), make_model()),
            Stage::new("impl".to_string(), make_model()),
        ];
        let mut bp = Blueprint::new("t".into(), "".into(), stages, make_layout());
        assert_eq!(bp.resolve_entry_stage_name(), "plan");

        bp.entry_stage = Some("impl".to_string());
        assert_eq!(bp.resolve_entry_stage_name(), "impl");
    }

    #[test]
    fn test_find_stage() {
        let stages = vec![
            Stage::new("plan".to_string(), make_model()),
            Stage::new("impl".to_string(), make_model()),
        ];
        let bp = Blueprint::new("t".into(), "".into(), stages, make_layout());
        assert!(bp.find_stage("plan").is_some());
        assert!(bp.find_stage("impl").is_some());
        assert!(bp.find_stage("nonexistent").is_none());
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

    // ─── stuck detection (#106) ─────────────────────────────────────────────

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

    /// A blueprint built programmatically (API / `lev validate`) bypasses the
    /// manifest parser, so `validate` has to catch the dead-edge shape too.
    #[test]
    fn validate_rejects_a_stuck_edge_with_no_threshold() {
        let build = |stuck| {
            let mut a = Stage::new("a".to_string(), make_model());
            let b = Stage::new("b".to_string(), make_model());
            let mut transitions = std::collections::HashMap::new();
            transitions.insert(
                "b".to_string(),
                TransitionEdge {
                    target: "b".to_string(),
                    condition: TransitionCondition::Stuck,
                    hint: None,
                    transform: EdgeTransform::Direct,
                    gate: None,
                    stuck,
                },
            );
            a.transitions = Some(transitions);
            Blueprint::new("t".into(), "".into(), vec![a, b], make_layout())
        };

        for dead in [None, Some(StuckConfig::default())] {
            let err = build(dead)
                .validate()
                .expect_err("dead stuck edge rejected");
            assert!(
                format!("{err:?}").contains("stuck_after_"),
                "unexpected error: {err:?}"
            );
        }

        // The same graph with a real threshold is fine.
        assert!(
            build(Some(StuckConfig {
                after_iterations: Some(5),
                ..Default::default()
            }))
            .validate()
            .is_ok()
        );
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
    fn test_has_terminal_path_unknown_stage_returns_false() {
        // `has_terminal_path` is private; this test is in the same module.
        // Calling it with a stage name that doesn't exist in the Blueprint
        // exercises the `None => return false` arm (blueprint.rs line 203).
        let stages = vec![Stage::new("start".to_string(), make_model())];
        let bp = Blueprint::new("t".into(), "d".into(), stages, make_layout());
        let mut visited = std::collections::HashSet::new();
        assert!(!bp.has_terminal_path("nonexistent_stage", &mut visited));
    }

    #[test]
    fn test_blueprint_validate_fails_when_layout_has_duplicate_region() {
        let regions = vec![
            RegionDefinition::new("dup".to_string(), RegionKind::Pinned, 100),
            RegionDefinition::new("dup".to_string(), RegionKind::Temporary, 100),
        ];
        let layout = ContextLayout::new(regions, 200);
        let stages = vec![Stage::new("start".to_string(), make_model())];
        let bp = Blueprint::new("t".into(), "d".into(), stages, layout);
        assert_eq!(
            bp.validate().unwrap_err(),
            ValidationError::Region {
                region: "dup".to_string(),
                message: "duplicate region name".to_string(),
            }
        );
    }

    #[test]
    fn test_blueprint_validate_fails_when_stage_has_empty_name() {
        let stages = vec![Stage::new("".to_string(), make_model())];
        let bp = Blueprint::new("t".into(), "d".into(), stages, make_layout());
        assert_eq!(
            bp.validate().unwrap_err(),
            ValidationError::Stage {
                stage: "(empty)".to_string(),
                message: "stage name cannot be empty".to_string(),
            }
        );
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
        assert!(routing.persist);
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
            persist: false,
            max_result_tokens: Some(4096),
            ..Default::default()
        };
        routing
            .tool_overrides
            .insert("read_file".to_string(), "file_reads".to_string());

        let json = serde_json::to_string(&routing).unwrap();
        let back: ToolResultRouting = serde_json::from_str(&json).unwrap();

        assert_eq!(back.default_region, "custom_region");
        assert!(!back.persist);
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
                persist: true,
                max_result_tokens: Some(2048),
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
        assert!(routing.persist);
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
        }
    }

    /// Blueprint: fan_out stage (worker_stage=fix_worker) → merge → terminal.
    /// The merge stage carries an (empty) transitions table so the blueprint is
    /// in graph mode - this makes `validate_graph` run `has_terminal_path`,
    /// which walks the fan-out stage's merge hand-off.
    fn fanout_blueprint(worker_allowed: bool, config: FanOutConfig) -> Blueprint {
        let mut fan = Stage::new("parallel".to_string(), make_model());
        fan.mode = StageMode::FanOut { config };
        let mut worker = Stage::new("fix_worker".to_string(), make_model());
        worker.allow_as_worker = worker_allowed;
        let mut merge = Stage::new("merge".to_string(), make_model());
        merge.transitions = Some(HashMap::new()); // terminal, graph mode
        Blueprint::new(
            "t".into(),
            "d".into(),
            vec![fan, worker, merge],
            make_layout(),
        )
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

    #[test]
    fn fanout_config_serde_roundtrip_and_max_workers_default() {
        let toml = r#"
worker_agent = "fixer"
split_prompt = "go"
on_worker_failure = "fail_all"
"#;
        let cfg: FanOutConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.worker_agent.as_deref(), Some("fixer"));
        assert_eq!(cfg.max_workers, 4); // default
        assert_eq!(cfg.on_worker_failure, WorkerFailurePolicy::FailAll);
        // JSON round-trip preserves everything.
        let json = serde_json::to_string(&fanout_config()).unwrap();
        let back: FanOutConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, fanout_config());
    }

    #[test]
    fn fanout_validate_ok_with_allowed_worker_stage() {
        assert!(fanout_blueprint(true, fanout_config()).validate().is_ok());
    }

    #[test]
    fn fanout_validate_rejects_worker_stage_not_opted_in() {
        let err = fanout_blueprint(false, fanout_config())
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("allow_as_worker"));
    }

    #[test]
    fn fanout_validate_rejects_missing_worker_stage() {
        let mut cfg = fanout_config();
        cfg.worker_stage = Some("nope".to_string());
        let err = fanout_blueprint(true, cfg).validate().unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn fanout_validate_rejects_missing_merge_stage() {
        let mut cfg = fanout_config();
        cfg.merge_stage = Some("nomerge".to_string());
        let err = fanout_blueprint(true, cfg).validate().unwrap_err();
        assert!(err.to_string().contains("merge_stage"));
    }

    #[test]
    fn fanout_validate_rejects_wrong_worker_source_count() {
        // zero sources
        let mut cfg = fanout_config();
        cfg.worker_stage = None;
        assert!(fanout_blueprint(true, cfg).validate().is_err());
        // two sources
        let mut cfg2 = fanout_config();
        cfg2.worker_agent = Some("x".to_string()); // plus worker_stage
        assert!(fanout_blueprint(true, cfg2).validate().is_err());
    }

    #[test]
    fn fanout_terminal_path_runs_through_merge_stage() {
        // worker_agent form (no local worker_stage), merge → terminal.
        let mut cfg = fanout_config();
        cfg.worker_stage = None;
        cfg.worker_agent = Some("external".to_string());
        assert!(fanout_blueprint(false, cfg).validate().is_ok());
    }

    #[test]
    fn fanout_validate_ok_without_merge_stage() {
        // No merge stage: valid, and the fan-out stage falls through to the
        // linear next stage for its terminal path.
        let mut cfg = fanout_config();
        cfg.merge_stage = None;
        assert!(fanout_blueprint(true, cfg).validate().is_ok());
    }
}
