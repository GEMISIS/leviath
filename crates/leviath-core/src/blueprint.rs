//! Agent blueprints and stage definitions.
//!
//! A blueprint is the complete definition of an agent type, including its
//! execution stages, model selection, tool access, and context layout.
//! Blueprints are typically defined in `leviath.toml` files and can be
//! shared, installed, and versioned.

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

    /// Additional metadata
    pub metadata: HashMap<String, serde_json::Value>,
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
            metadata: HashMap::new(),
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
    pub fn validate(&self) -> Result<(), String> {
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

        Ok(())
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

/// Style of interaction at an interaction point.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
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

/// A point where a stage can request user input.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() {
            return Err("Stage name cannot be empty".to_string());
        }

        // Validate stage-specific context layout if present
        if let Some(layout) = &self.context_layout {
            layout.validate()?;
        }

        Ok(())
    }
}

/// Model configuration for a stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Provider name (e.g., "anthropic", "openai")
    pub provider: String,

    /// Model identifier (e.g., "claude-sonnet-4-5")
    pub model: String,

    /// Optional parameters for this model
    pub parameters: HashMap<String, serde_json::Value>,
}

impl ModelConfig {
    /// Create a new model configuration.
    pub fn new(provider: String, model: String) -> Self {
        Self {
            provider,
            model,
            parameters: HashMap::new(),
        }
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
    fn validate(&self, layout: &ContextLayout) -> Result<(), String> {
        for mapping in &self.mappings {
            // We can only validate target regions against the current layout
            // (source regions belong to a different blueprint)
            if layout.get_region(&mapping.to_region).is_none() {
                return Err(format!(
                    "Transform target region '{}' not found in layout",
                    mapping.to_region
                ));
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
    use crate::region::RegionKind;
    use crate::layout::RegionDefinition;

    #[test]
    fn test_blueprint_creation() {
        let regions = vec![
            RegionDefinition::new("test".to_string(), RegionKind::Pinned, 5000),
        ];
        let layout = ContextLayout::new(regions, 10000);
        
        let stages = vec![
            Stage::new("analyze".to_string(), ModelConfig::new("anthropic".to_string(), "claude-sonnet-4".to_string())),
        ];
        
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
    fn test_stage_validation() {
        let stage = Stage::new("test".to_string(), ModelConfig::new("anthropic".to_string(), "claude-sonnet-4".to_string()));
        assert!(stage.validate().is_ok());

        let empty_stage = Stage::new("".to_string(), ModelConfig::new("anthropic".to_string(), "claude-sonnet-4".to_string()));
        assert!(empty_stage.validate().is_err());
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
        routing.tool_overrides.insert("read_file".to_string(), "codebase".to_string());
        routing.tool_overrides.insert("search".to_string(), "findings".to_string());
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
            ModelConfig::new("anthropic".to_string(), "claude-sonnet-4".to_string()),
        );

        let mut routing = ToolResultRouting::default();
        routing.default_region = "tool_results".to_string();
        routing.persist = true;
        routing.max_result_tokens = Some(5000);
        stage.tool_result_routing = Some(routing);

        assert!(stage.tool_result_routing.is_some());
        let r = stage.tool_result_routing.unwrap();
        assert_eq!(r.max_result_tokens, Some(5000));
    }
}
