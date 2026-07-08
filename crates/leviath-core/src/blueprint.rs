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

/// An agent blueprint — the complete definition of an agent type.
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

    /// Tool filters controlling which tools are available
    pub tools: Vec<ToolFilter>,

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
            tools: Vec::new(),
            transforms: Vec::new(),
            version: "0.1.0".to_string(),
            compaction_config: None,
            max_child_depth: None,
            entry_stage: None,
            metadata: HashMap::new(),
            security: None,
        }
    }

    /// Add tool filters to this blueprint.
    pub fn with_tools(mut self, tools: Vec<ToolFilter>) -> Self {
        self.tools = tools;
        self
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
        if let Some(ref entry) = self.entry_stage {
            if !stage_names.contains(entry.as_str()) {
                return Err(ValidationError::Graph(format!(
                    "entry_stage '{}' does not match any defined stage",
                    entry
                )));
            }
        }

        let has_any_transitions = self.stages.iter().any(|s| s.transitions.is_some());
        if !has_any_transitions {
            // Pure linear mode — no graph validation needed
            return Ok(());
        }

        // All transition targets must exist
        for stage in &self.stages {
            if let Some(ref transitions) = stage.transitions {
                for target_name in transitions.keys() {
                    if !stage_names.contains(target_name.as_str()) {
                        return Err(ValidationError::Transition {
                            from: stage.name.clone(),
                            to: target_name.clone(),
                            message: "target stage does not exist".to_string(),
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
                "no terminal path exists from entry stage — agent would never complete".to_string(),
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
                let all_exhaustible = transitions.keys().all(|target| {
                    self.stages
                        .iter()
                        .find(|s| s.name == *target)
                        .map(|s| s.max_revisits.is_some())
                        .unwrap_or(false)
                });
                all_exhaustible
            }
        }
    }

    /// Find a stage by name.
    pub fn find_stage(&self, name: &str) -> Option<&Stage> {
        self.stages.iter().find(|s| s.name == name)
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
}

impl PartialEq for StageMode {
    #[inline(never)]
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Autonomous, Self::Autonomous) | (Self::Interactive, Self::Interactive) => true,
            (Self::InteractivePoints { points: a }, Self::InteractivePoints { points: b }) => {
                a == b
            }
            _ => false,
        }
    }
}
impl Eq for StageMode {}

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

    /// Follow-up free-text prompts, keyed by option label.
    ///
    /// When the user picks an option present in this map (e.g. "Revise — I'll
    /// describe changes"), a second `FreeText` interaction is requested using
    /// the mapped prompt so the user can actually describe what they want —
    /// otherwise only the static option label ever reaches the model, and
    /// the user's intent is lost. After the follow-up is answered, the stage
    /// runs another inference segment and re-prompts the same interaction
    /// point (bounded by a retry cap) instead of falling through.
    #[serde(default)]
    pub followups: HashMap<String, String>,
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

    /// Optional routing configuration for tool results
    pub tool_result_routing: Option<ToolResultRouting>,

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
    /// transition target — e.g. a review stage that approves the work
    /// needs no further stage. When true, `prompt_llm_transition`'s query
    /// offers an explicit "DONE" response that resolves to a terminal
    /// (no-transition) outcome instead of forcing the single/first
    /// available edge.
    #[serde(default)]
    pub allow_complete: bool,
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
            tool_result_routing: None,
            tool_permissions: HashMap::new(),
            requires_children: false,
            transitions: None,
            max_revisits: None,
            transition_prompt: None,
            accepts_messages: true,
            allow_complete: false,
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

/// Filter controlling which tools an agent can access.
///
/// Tool filters can include or exclude specific tools, tool categories,
/// or apply more complex rules for tool access.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFilter {
    /// Name or pattern for tools
    pub pattern: String,

    /// Whether this is an include or exclude filter
    pub filter_type: FilterType,

    /// Optional stage restriction
    pub stage: Option<String>,
}

/// Type of tool filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FilterType {
    /// Include matching tools
    Include,
    /// Exclude matching tools
    Exclude,
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
}

/// Condition that determines when a transition edge is available.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// Custom condition string (future: Rhai expression)
    Custom(String),
}

impl PartialEq for TransitionCondition {
    #[inline(never)]
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Always, Self::Always)
            | (Self::Error, Self::Error)
            | (Self::MaxIterations, Self::MaxIterations)
            | (Self::LlmChoice, Self::LlmChoice) => true,
            (Self::Custom(a), Self::Custom(b)) => a == b,
            _ => false,
        }
    }
}
impl Eq for TransitionCondition {}

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

/// Result of running a stage, used for transition condition evaluation.
#[derive(Debug, Clone)]
pub enum StageResult {
    /// Stage completed normally
    Success,
    /// Stage encountered an error
    Error,
    /// Stage hit max_iterations without LLM signaling completion
    MaxIterations,
}

impl PartialEq for StageResult {
    #[inline(never)]
    fn eq(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}
impl Eq for StageResult {}

/// Content transformation type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentTransform {
    /// Copy content as-is
    Direct,

    /// Summarize content to fit target region
    Summarize,

    /// Extract specific fields
    Extract { fields: Vec<String> },

    /// Custom transformation
    Custom { function: String },
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
    fn test_blueprint_with_tools_transforms_version() {
        let stages = vec![Stage::new("plan".to_string(), make_model())];
        let bp = Blueprint::new("t".into(), "d".into(), stages, make_layout())
            .with_tools(vec![ToolFilter {
                pattern: "bash".to_string(),
                filter_type: FilterType::Exclude,
                stage: None,
            }])
            .with_transforms(vec![ContextTransform {
                from_blueprint: "a".to_string(),
                to_blueprint: "b".to_string(),
                mappings: vec![],
            }])
            .with_version("2.0.0".to_string());

        assert_eq!(bp.tools.len(), 1);
        assert_eq!(bp.transforms.len(), 1);
        assert_eq!(bp.version, "2.0.0");
    }

    #[test]
    fn test_blueprint_validate_runs_transform_validation() {
        // A transform whose mapping targets a real region — validate() must
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
        // but "impl" and "review" have none — they must fall back to
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
            "tool_result_routing": null,
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
    fn test_interaction_point_followups_default_empty() {
        let point = InteractionPoint {
            name: "plan_approval".to_string(),
            prompt: "Approve?".to_string(),
            required: true,
            style: InteractionStyle::MultipleChoice,
            options: vec!["Approve".to_string(), "Revise".to_string()],
            followups: HashMap::new(),
        };
        assert!(point.followups.is_empty());
    }

    #[test]
    fn test_interaction_point_followups_roundtrip() {
        let mut followups = HashMap::new();
        followups.insert(
            "Revise".to_string(),
            "What would you like to change?".to_string(),
        );
        let point = InteractionPoint {
            name: "plan_approval".to_string(),
            prompt: "Approve?".to_string(),
            required: true,
            style: InteractionStyle::MultipleChoice,
            options: vec!["Approve".to_string(), "Revise".to_string()],
            followups,
        };
        let json = serde_json::to_string(&point).unwrap();
        let back: InteractionPoint = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.followups.get("Revise").map(|s| s.as_str()),
            Some("What would you like to change?")
        );
    }

    #[test]
    fn test_interaction_point_followups_serde_default_when_missing() {
        let json = r#"{
            "name": "plan_approval",
            "prompt": "Approve?",
            "required": true,
            "style": "multiple_choice",
            "options": ["Approve", "Revise"]
        }"#;
        let point: InteractionPoint = serde_json::from_str(json).unwrap();
        assert!(point.followups.is_empty());
    }

    #[test]
    fn test_tool_result_routing_default() {
        let routing = ToolResultRouting::default();
        assert_eq!(routing.default_region, "tool_results");
        assert!(routing.persist);
        assert!(routing.max_result_tokens.is_none());
        assert!(routing.tool_overrides.is_empty());
    }

    #[test]
    fn test_tool_result_routing_with_overrides() {
        let mut routing = ToolResultRouting::default();
        routing
            .tool_overrides
            .insert("read_file".to_string(), "codebase".to_string());
        routing
            .tool_overrides
            .insert("search".to_string(), "findings".to_string());
        routing.max_result_tokens = Some(5000);
        routing.persist = false;

        assert_eq!(routing.tool_overrides.len(), 2);
        assert_eq!(routing.tool_overrides.get("read_file").unwrap(), "codebase");
        assert!(!routing.persist);
        assert_eq!(routing.max_result_tokens, Some(5000));
    }

    #[test]
    fn test_stage_with_tool_result_routing() {
        let mut stage = Stage::new(
            "implement".to_string(),
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4-6".to_string()),
        );

        let routing = ToolResultRouting {
            default_region: "tool_results".to_string(),
            persist: true,
            max_result_tokens: Some(5000),
            ..Default::default()
        };
        stage.tool_result_routing = Some(routing);

        assert!(stage.tool_result_routing.is_some());
        let r = stage.tool_result_routing.unwrap();
        assert_eq!(r.max_result_tokens, Some(5000));
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
        // Minimal JSON — models defaults to empty, allow_user_default defaults to true
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
            },
        );
        stage.transitions = Some(transitions);
        let bp = Blueprint::new("t".into(), "".into(), vec![stage], make_layout());
        assert!(bp.validate().is_err());
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
            },
        );
        b.transitions = Some(b_transitions);

        let bp = Blueprint::new("t".into(), "".into(), vec![a, b], make_layout());
        assert!(bp.validate().is_err());
    }

    #[test]
    fn test_linear_stages_still_validate() {
        // No transitions set at all — pure linear mode
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
    fn test_stage_result_variants() {
        assert_eq!(StageResult::Success, StageResult::Success);
        assert_ne!(StageResult::Error, StageResult::Success);
        assert_ne!(StageResult::MaxIterations, StageResult::Error);
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

    #[test]
    fn test_transition_condition_custom_equality() {
        let a = TransitionCondition::Custom("x".to_string());
        let b = TransitionCondition::Custom("x".to_string());
        assert_eq!(a, b);
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
}
