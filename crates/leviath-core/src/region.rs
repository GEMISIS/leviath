//! Memory region types and validation schemas.
//!
//! Regions are typed sections of an agent's context window with different lifecycle
//! policies. This module defines the region kinds, content storage, and validation
//! schemas that enforce content format requirements.

use serde::{Deserialize, Serialize};

/// A typed memory region within an agent's context window.
///
/// Regions have different lifecycle policies controlling how they behave
/// when the context window fills up. This is inspired by hardware memory
/// architectures like SNES VRAM, where different memory regions serve
/// distinct purposes with their own access patterns and constraints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RegionKind {
    /// Never evicted or compacted. Architecture diagrams, constraints, identity.
    ///
    /// Like SNES OAM (Object Attribute Memory) — fixed format, always present.
    /// Use for content that defines the agent's core identity, constraints,
    /// and architectural understanding. This content persists for the entire
    /// agent lifecycle.
    Pinned,

    /// Maintains the last N items, oldest rolls off. Conversation history.
    ///
    /// Like a ring buffer with configurable size. When the buffer is full,
    /// the oldest item is removed to make room for new content. Use for
    /// conversation history or any sequential data where recent items
    /// are most relevant.
    SlidingWindow {
        /// Maximum number of items to retain in the window
        max_items: usize,
    },

    /// First to be evicted when space is needed. Tool outputs, intermediate results.
    ///
    /// Cheapest to regenerate, lowest priority to keep. Use for content that
    /// can be easily regenerated or has low value after immediate use, such as
    /// tool execution results or temporary computations.
    Temporary,

    /// Compacts (summarizes) when threshold is hit, but never fully evicted.
    ///
    /// Retains compressed form of historical context. When token count exceeds
    /// the threshold, the region's content is summarized to reduce token usage
    /// while preserving essential information. The summarized version remains
    /// in the context indefinitely.
    Compacting {
        /// Token count that triggers compaction
        threshold_tokens: usize,
    },
}

/// A single region in the context window with its content and metadata.
///
/// Each region tracks its own token budget, current usage, and optional
/// validation schema to enforce content format requirements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Region {
    /// Unique name identifying this region
    pub name: String,

    /// Lifecycle policy for this region
    pub kind: RegionKind,

    /// Content entries stored in this region
    pub content: Vec<RegionEntry>,

    /// Maximum tokens allowed in this region
    pub max_tokens: usize,

    /// Current token count
    pub current_tokens: usize,

    /// Optional validation schema enforcing content format
    pub schema: Option<RegionSchema>,
}

impl Region {
    /// Create a new region with the specified configuration.
    pub fn new(name: String, kind: RegionKind, max_tokens: usize) -> Self {
        Self {
            name,
            kind,
            content: Vec::new(),
            max_tokens,
            current_tokens: 0,
            schema: None,
        }
    }

    /// Add a validation schema to this region.
    pub fn with_schema(mut self, schema: RegionSchema) -> Self {
        self.schema = Some(schema);
        self
    }
}

/// A single entry within a region.
///
/// Each entry has content and metadata tracking its token usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionEntry {
    /// The actual content of this entry
    pub content: String,

    /// Token count for this entry
    pub tokens: usize,

    /// Timestamp when this entry was added
    pub timestamp: i64,

    /// Optional metadata about this entry
    pub metadata: Option<serde_json::Value>,
}

/// Validation schema for a region's content.
///
/// Enforces that content matches expected format (e.g., mermaid diagrams only,
/// JSON only, code only). Schemas can include multiple validators that are
/// checked when content is added to a region.
#[derive(Debug, Serialize, Deserialize)]
pub struct RegionSchema {
    /// Expected content format
    pub format: ContentFormat,
}

impl Clone for RegionSchema {
    fn clone(&self) -> Self {
        Self {
            format: self.format.clone(),
        }
    }
}

/// Content format types that can be enforced via schemas.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentFormat {
    /// Plain text, no formatting requirements
    Text,

    /// Valid JSON
    Json,

    /// Mermaid diagram syntax
    Mermaid,

    /// Source code in a specific language
    Code { language: String },

    /// Markdown formatted text
    Markdown,

    /// Custom format with user-defined validation
    Custom { format_name: String },
}

/// Trait for content validators.
///
/// Validators check whether content meets specific requirements before
/// it's added to a region. This enables enforcing architectural constraints
/// like "only mermaid diagrams in the architecture region".
pub trait Validator: Send + Sync {
    /// Validate content and return an error message if invalid.
    fn validate(&self, content: &str) -> Result<(), String>;

    /// Get a description of what this validator checks.
    fn description(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_region_creation() {
        let region = Region::new("test".to_string(), RegionKind::Pinned, 1000);
        assert_eq!(region.name, "test");
        assert_eq!(region.max_tokens, 1000);
        assert_eq!(region.current_tokens, 0);
    }

    #[test]
    fn test_sliding_window_config() {
        let kind = RegionKind::SlidingWindow { max_items: 10 };
        let region = Region::new("history".to_string(), kind, 5000);
        match region.kind {
            RegionKind::SlidingWindow { max_items } => assert_eq!(max_items, 10),
            _ => panic!("Wrong region kind"),
        }
    }
}
