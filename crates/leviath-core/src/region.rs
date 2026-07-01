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

    /// Compacts (summarizes) when threshold is hit, then cleared.
    ///
    /// When token count exceeds the threshold, the region's content is summarized
    /// and moved to a paired CompactHistory region, then the original Compacting
    /// region is completely cleared, giving fresh capacity.
    Compacting {
        /// Token count that triggers compaction
        threshold_tokens: usize,
    },

    /// Wiped entirely in one shot when space is needed. All-or-nothing eviction.
    ///
    /// Unlike Temporary (which evicts oldest entries one at a time), Clearable
    /// regions are dumped completely and immediately when eviction is needed.
    /// Use for scratch space or temporary working data where partial results
    /// are useless.
    Clearable,

    /// Receives summaries from paired Compacting regions, never evicted.
    ///
    /// When a Compacting region hits its threshold and summarizes, the summary
    /// moves here. CompactHistory regions hold compressed knowledge indefinitely
    /// and are never evicted. Can also support sliding window behavior (oldest
    /// summaries drop off) and re-compaction (combine multiple summaries).
    CompactHistory {
        /// Name of the source Compacting region
        source_region: String,
    },
}

impl RegionKind {
    /// Return the cache hint appropriate for this region kind.
    pub fn cache_hint(&self) -> crate::cache::CacheHint {
        match self {
            RegionKind::Pinned | RegionKind::CompactHistory { .. } => {
                crate::cache::CacheHint::Always
            }
            RegionKind::Compacting { .. } => crate::cache::CacheHint::UntilChanged,
            RegionKind::SlidingWindow { .. } => crate::cache::CacheHint::SlidingPrefix {
                stable_fraction: 0.75,
            },
            RegionKind::Temporary | RegionKind::Clearable => crate::cache::CacheHint::Never,
        }
    }
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

    /// Add an entry to this region.
    ///
    /// Validates content against schema if present, checks token budget,
    /// and adds the entry to the region.
    pub fn add_entry(&mut self, content: String, tokens: usize) -> crate::error::Result<()> {
        // Validate against schema if present
        if let Some(schema) = &self.schema {
            schema.validate(&content)?;
        }

        // Check token budget
        if self.current_tokens + tokens > self.max_tokens {
            return Err(crate::error::Error::TokenBudgetExceeded {
                used: self.current_tokens + tokens,
                max: self.max_tokens,
            });
        }

        // Add entry
        self.content.push(RegionEntry {
            content,
            tokens,
            timestamp: chrono::Utc::now().timestamp(),
            metadata: None,
        });
        self.current_tokens += tokens;

        // Enforce SlidingWindow max_items limit
        self.enforce_sliding_window();

        Ok(())
    }

    /// Add an entry with metadata.
    pub fn add_entry_with_metadata(
        &mut self,
        content: String,
        tokens: usize,
        metadata: serde_json::Value,
    ) -> crate::error::Result<()> {
        // Validate against schema if present
        if let Some(schema) = &self.schema {
            schema.validate(&content)?;
        }

        // Check token budget
        if self.current_tokens + tokens > self.max_tokens {
            return Err(crate::error::Error::TokenBudgetExceeded {
                used: self.current_tokens + tokens,
                max: self.max_tokens,
            });
        }

        // Add entry
        self.content.push(RegionEntry {
            content,
            tokens,
            timestamp: chrono::Utc::now().timestamp(),
            metadata: Some(metadata),
        });
        self.current_tokens += tokens;

        // Enforce SlidingWindow max_items limit
        self.enforce_sliding_window();

        Ok(())
    }

    /// Enforce the SlidingWindow max_items limit by removing oldest entries.
    fn enforce_sliding_window(&mut self) {
        if let RegionKind::SlidingWindow { max_items } = &self.kind {
            let max = *max_items;
            while self.content.len() > max {
                if let Some(removed) = self.content.first() {
                    self.current_tokens -= removed.tokens;
                }
                self.content.remove(0);
            }
        }
    }

    /// Clear all content from this region.
    pub fn clear(&mut self) {
        self.content.clear();
        self.current_tokens = 0;
    }

    /// Remove the oldest entry (for Temporary regions).
    pub fn remove_oldest(&mut self) -> Option<RegionEntry> {
        if let Some(entry) = self.content.first() {
            self.current_tokens -= entry.tokens;
            Some(self.content.remove(0))
        } else {
            None
        }
    }

    /// Get the number of entries in this region.
    pub fn entry_count(&self) -> usize {
        self.content.len()
    }

    /// Check if region needs compaction (for Compacting regions).
    pub fn needs_compaction(&self) -> bool {
        if let RegionKind::Compacting { threshold_tokens } = self.kind {
            self.current_tokens > threshold_tokens
        } else {
            false
        }
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

    /// Optional custom validation script (Rhai)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_script: Option<String>,
}

impl Clone for RegionSchema {
    fn clone(&self) -> Self {
        Self {
            format: self.format.clone(),
            custom_script: self.custom_script.clone(),
        }
    }
}

impl RegionSchema {
    /// Create a new schema with the specified format.
    pub fn new(format: ContentFormat) -> Self {
        Self {
            format,
            custom_script: None,
        }
    }

    /// Add a custom validation script.
    pub fn with_custom_script(mut self, script: String) -> Self {
        self.custom_script = Some(script);
        self
    }

    /// Validate content against this schema.
    pub fn validate(&self, content: &str) -> crate::error::Result<()> {
        match &self.format {
            ContentFormat::Json => {
                serde_json::from_str::<serde_json::Value>(content).map_err(|e| {
                    crate::error::Error::ValidationFailed(format!("Invalid JSON: {}", e))
                })?;
            }
            ContentFormat::Mermaid => {
                // Basic mermaid syntax validation
                if !content.contains("graph")
                    && !content.contains("sequenceDiagram")
                    && !content.contains("classDiagram")
                    && !content.contains("stateDiagram")
                    && !content.contains("erDiagram")
                    && !content.contains("journey")
                    && !content.contains("gantt")
                    && !content.contains("pie")
                    && !content.contains("flowchart")
                {
                    return Err(crate::error::Error::ValidationFailed(
                        "Mermaid diagrams must contain a valid diagram type (graph, sequenceDiagram, etc.)".to_string()
                    ));
                }
            }
            ContentFormat::Code { .. } => {
                // Basic code validation - just check it's not empty
                if content.trim().is_empty() {
                    return Err(crate::error::Error::ValidationFailed(
                        "Code cannot be empty".to_string(),
                    ));
                }
            }
            ContentFormat::Markdown => {
                // Markdown is very permissive, just check it's not empty
                if content.trim().is_empty() {
                    return Err(crate::error::Error::ValidationFailed(
                        "Markdown content cannot be empty".to_string(),
                    ));
                }
            }
            ContentFormat::Text | ContentFormat::Custom { .. } => {
                // Text has no restrictions, Custom is handled by scripting layer
            }
        }

        Ok(())
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
    fn validate(&self, content: &str) -> std::result::Result<(), crate::error::ValidationError>;

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

    #[test]
    fn test_sliding_window_enforces_max_items() {
        let mut region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow { max_items: 3 },
            50000,
        );

        region.add_entry("msg1".to_string(), 10).unwrap();
        region.add_entry("msg2".to_string(), 20).unwrap();
        region.add_entry("msg3".to_string(), 30).unwrap();
        assert_eq!(region.entry_count(), 3);
        assert_eq!(region.current_tokens, 60);

        // Adding a 4th entry should evict the oldest
        region.add_entry("msg4".to_string(), 40).unwrap();
        assert_eq!(region.entry_count(), 3);
        assert_eq!(region.content[0].content, "msg2");
        assert_eq!(region.content[2].content, "msg4");
        assert_eq!(region.current_tokens, 90); // 20 + 30 + 40

        // Adding a 5th entry should evict again
        region.add_entry("msg5".to_string(), 50).unwrap();
        assert_eq!(region.entry_count(), 3);
        assert_eq!(region.content[0].content, "msg3");
        assert_eq!(region.current_tokens, 120); // 30 + 40 + 50
    }

    #[test]
    fn test_sliding_window_enforces_max_items_with_metadata() {
        let mut region = Region::new(
            "conv".to_string(),
            RegionKind::SlidingWindow { max_items: 2 },
            50000,
        );

        region
            .add_entry_with_metadata("a".to_string(), 10, serde_json::json!({"idx": 1}))
            .unwrap();
        region
            .add_entry_with_metadata("b".to_string(), 20, serde_json::json!({"idx": 2}))
            .unwrap();
        region
            .add_entry_with_metadata("c".to_string(), 30, serde_json::json!({"idx": 3}))
            .unwrap();

        assert_eq!(region.entry_count(), 2);
        assert_eq!(region.content[0].content, "b");
        assert_eq!(region.content[1].content, "c");
        assert_eq!(region.current_tokens, 50);
    }

    #[test]
    fn test_cache_hint_pinned() {
        let kind = RegionKind::Pinned;
        assert_eq!(kind.cache_hint(), crate::cache::CacheHint::Always);
    }

    #[test]
    fn test_cache_hint_compact_history() {
        let kind = RegionKind::CompactHistory {
            source_region: "conv".to_string(),
        };
        assert_eq!(kind.cache_hint(), crate::cache::CacheHint::Always);
    }

    #[test]
    fn test_cache_hint_compacting() {
        let kind = RegionKind::Compacting {
            threshold_tokens: 1000,
        };
        assert_eq!(kind.cache_hint(), crate::cache::CacheHint::UntilChanged);
    }

    #[test]
    fn test_cache_hint_sliding_window() {
        let kind = RegionKind::SlidingWindow { max_items: 10 };
        match kind.cache_hint() {
            crate::cache::CacheHint::SlidingPrefix { stable_fraction } => {
                assert!((stable_fraction - 0.75).abs() < f32::EPSILON);
            }
            other => panic!("Expected SlidingPrefix, got {:?}", other),
        }
    }

    #[test]
    fn test_cache_hint_temporary() {
        assert_eq!(
            RegionKind::Temporary.cache_hint(),
            crate::cache::CacheHint::Never
        );
    }

    #[test]
    fn test_cache_hint_clearable() {
        assert_eq!(
            RegionKind::Clearable.cache_hint(),
            crate::cache::CacheHint::Never
        );
    }

    // ─── Region::with_schema / add_entry schema + budget checks ────────────

    #[test]
    fn test_with_schema_attaches_schema() {
        let schema = RegionSchema::new(ContentFormat::Json);
        let region =
            Region::new("data".to_string(), RegionKind::Temporary, 1000).with_schema(schema);
        assert!(region.schema.is_some());
    }

    #[test]
    fn test_add_entry_rejects_content_failing_schema() {
        let schema = RegionSchema::new(ContentFormat::Json);
        let mut region =
            Region::new("data".to_string(), RegionKind::Temporary, 1000).with_schema(schema);
        let result = region.add_entry("not json".to_string(), 10);
        assert!(result.is_err());
        assert_eq!(region.entry_count(), 0);
    }

    #[test]
    fn test_add_entry_accepts_content_passing_schema() {
        let schema = RegionSchema::new(ContentFormat::Json);
        let mut region =
            Region::new("data".to_string(), RegionKind::Temporary, 1000).with_schema(schema);
        let result = region.add_entry("{\"a\":1}".to_string(), 10);
        assert!(result.is_ok());
        assert_eq!(region.entry_count(), 1);
    }

    #[test]
    fn test_add_entry_rejects_over_budget() {
        let mut region = Region::new("data".to_string(), RegionKind::Temporary, 10);
        let result = region.add_entry("too much".to_string(), 20);
        assert!(matches!(
            result,
            Err(crate::error::Error::TokenBudgetExceeded { used: 20, max: 10 })
        ));
        assert_eq!(region.entry_count(), 0);
    }

    #[test]
    fn test_add_entry_with_metadata_rejects_content_failing_schema() {
        let schema = RegionSchema::new(ContentFormat::Json);
        let mut region =
            Region::new("data".to_string(), RegionKind::Temporary, 1000).with_schema(schema);
        let result =
            region.add_entry_with_metadata("not json".to_string(), 10, serde_json::json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn test_add_entry_with_metadata_rejects_over_budget() {
        let mut region = Region::new("data".to_string(), RegionKind::Temporary, 10);
        let result =
            region.add_entry_with_metadata("too much".to_string(), 20, serde_json::json!({}));
        assert!(matches!(
            result,
            Err(crate::error::Error::TokenBudgetExceeded { used: 20, max: 10 })
        ));
    }

    #[test]
    fn test_add_entry_with_metadata_stores_metadata() {
        let mut region = Region::new("data".to_string(), RegionKind::Temporary, 1000);
        region
            .add_entry_with_metadata("hello".to_string(), 5, serde_json::json!({"k": "v"}))
            .unwrap();
        assert_eq!(
            region.content[0].metadata,
            Some(serde_json::json!({"k": "v"}))
        );
    }

    // ─── clear / remove_oldest / needs_compaction ──────────────────────────

    #[test]
    fn test_clear_removes_all_content_and_resets_tokens() {
        let mut region = Region::new("data".to_string(), RegionKind::Temporary, 1000);
        region.add_entry("a".to_string(), 10).unwrap();
        region.add_entry("b".to_string(), 20).unwrap();
        assert_eq!(region.entry_count(), 2);

        region.clear();
        assert_eq!(region.entry_count(), 0);
        assert_eq!(region.current_tokens, 0);
    }

    #[test]
    fn test_remove_oldest_returns_and_removes_first_entry() {
        let mut region = Region::new("data".to_string(), RegionKind::Temporary, 1000);
        region.add_entry("first".to_string(), 10).unwrap();
        region.add_entry("second".to_string(), 20).unwrap();

        let removed = region.remove_oldest().unwrap();
        assert_eq!(removed.content, "first");
        assert_eq!(region.entry_count(), 1);
        assert_eq!(region.current_tokens, 20);
    }

    #[test]
    fn test_remove_oldest_returns_none_when_empty() {
        let mut region = Region::new("data".to_string(), RegionKind::Temporary, 1000);
        assert!(region.remove_oldest().is_none());
    }

    #[test]
    fn test_needs_compaction_true_when_over_threshold() {
        let mut region = Region::new(
            "impl".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 10,
            },
            1000,
        );
        region.add_entry("x".to_string(), 20).unwrap();
        assert!(region.needs_compaction());
    }

    #[test]
    fn test_needs_compaction_false_when_under_threshold() {
        let mut region = Region::new(
            "impl".to_string(),
            RegionKind::Compacting {
                threshold_tokens: 100,
            },
            1000,
        );
        region.add_entry("x".to_string(), 20).unwrap();
        assert!(!region.needs_compaction());
    }

    #[test]
    fn test_needs_compaction_false_for_non_compacting_kind() {
        let region = Region::new("data".to_string(), RegionKind::Temporary, 1000);
        assert!(!region.needs_compaction());
    }

    // ─── RegionSchema::with_custom_script ──────────────────────────────────

    #[test]
    fn test_region_schema_with_custom_script() {
        let schema = RegionSchema::new(ContentFormat::Custom {
            format_name: "special".to_string(),
        })
        .with_custom_script("validate_special()".to_string());
        assert_eq!(schema.custom_script.as_deref(), Some("validate_special()"));
    }

    // ─── RegionSchema::validate — every ContentFormat branch ───────────────

    #[test]
    fn test_validate_json_valid() {
        let schema = RegionSchema::new(ContentFormat::Json);
        assert!(schema.validate("{\"a\": 1}").is_ok());
    }

    #[test]
    fn test_validate_json_invalid() {
        let schema = RegionSchema::new(ContentFormat::Json);
        let err = schema.validate("not json").unwrap_err();
        assert!(matches!(err, crate::error::Error::ValidationFailed(_)));
    }

    #[test]
    fn test_validate_mermaid_valid() {
        let schema = RegionSchema::new(ContentFormat::Mermaid);
        assert!(schema.validate("graph TD\nA-->B").is_ok());
    }

    #[test]
    fn test_validate_mermaid_all_recognized_diagram_types() {
        let schema = RegionSchema::new(ContentFormat::Mermaid);
        for kind in [
            "graph",
            "sequenceDiagram",
            "classDiagram",
            "stateDiagram",
            "erDiagram",
            "journey",
            "gantt",
            "pie",
            "flowchart",
        ] {
            assert!(
                schema.validate(&format!("{} content", kind)).is_ok(),
                "{} should be recognized as valid mermaid",
                kind
            );
        }
    }

    #[test]
    fn test_validate_mermaid_invalid() {
        let schema = RegionSchema::new(ContentFormat::Mermaid);
        let err = schema.validate("just some text").unwrap_err();
        assert!(matches!(err, crate::error::Error::ValidationFailed(_)));
    }

    #[test]
    fn test_validate_code_non_empty_is_ok() {
        let schema = RegionSchema::new(ContentFormat::Code {
            language: "rust".to_string(),
        });
        assert!(schema.validate("fn main() {}").is_ok());
    }

    #[test]
    fn test_validate_code_empty_is_error() {
        let schema = RegionSchema::new(ContentFormat::Code {
            language: "rust".to_string(),
        });
        let err = schema.validate("   ").unwrap_err();
        assert!(matches!(err, crate::error::Error::ValidationFailed(_)));
    }

    #[test]
    fn test_validate_markdown_non_empty_is_ok() {
        let schema = RegionSchema::new(ContentFormat::Markdown);
        assert!(schema.validate("# Heading").is_ok());
    }

    #[test]
    fn test_validate_markdown_empty_is_error() {
        let schema = RegionSchema::new(ContentFormat::Markdown);
        let err = schema.validate("").unwrap_err();
        assert!(matches!(err, crate::error::Error::ValidationFailed(_)));
    }

    #[test]
    fn test_validate_text_has_no_restrictions() {
        let schema = RegionSchema::new(ContentFormat::Text);
        assert!(schema.validate("").is_ok());
        assert!(schema.validate("anything at all").is_ok());
    }

    #[test]
    fn test_validate_custom_has_no_restrictions_here() {
        let schema = RegionSchema::new(ContentFormat::Custom {
            format_name: "special".to_string(),
        });
        // Custom format validation is deferred to the scripting layer —
        // this schema's own validate() is a no-op for it.
        assert!(schema.validate("").is_ok());
        assert!(schema.validate("whatever").is_ok());
    }

    // ─── RegionSchema Clone impl ────────────────────────────────────────────

    #[test]
    fn test_region_schema_clone_preserves_fields() {
        let schema = RegionSchema::new(ContentFormat::Text).with_custom_script("s".to_string());
        let cloned = schema.clone();
        assert_eq!(cloned.custom_script.as_deref(), Some("s"));
        assert!(matches!(cloned.format, ContentFormat::Text));
    }
}
