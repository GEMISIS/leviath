//! Context window layouts and memory maps.
//!
//! A layout defines the complete memory structure for an agent's context window,
//! including all regions, their sizes, and eviction priorities. This is analogous
//! to a hardware memory map that defines where different types of data live and
//! how they're managed.

use crate::region::{RegionKind, RegionSchema};
use serde::{Deserialize, Serialize};

/// A ContextLayout defines the complete memory map for an agent.
///
/// Like SNES VRAM layout — every region has a defined purpose, size, and policy.
/// The layout specifies:
/// - Which regions exist and their configurations
/// - Total token budget across all regions
/// - Eviction order when space is needed
///
/// Layouts are typically defined in an agent's blueprint and remain constant
/// throughout the agent's lifecycle, though the content within regions changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextLayout {
    /// All regions in this layout
    pub regions: Vec<RegionDefinition>,

    /// Total token budget across all regions
    pub total_budget_tokens: usize,

    /// Region names in eviction priority order (first = evicted first)
    ///
    /// When the context window fills up, regions are processed in this order:
    /// 1. Temporary regions: evict oldest entries
    /// 2. Compacting regions: trigger summarization
    /// 3. SlidingWindow regions: reduce window size
    /// 4. Pinned regions: NEVER touched (if these fill up, it's a config error)
    pub eviction_order: Vec<String>,
}

impl ContextLayout {
    /// Create a new layout with the specified configuration.
    pub fn new(regions: Vec<RegionDefinition>, total_budget_tokens: usize) -> Self {
        Self {
            regions,
            total_budget_tokens,
            eviction_order: Vec::new(),
        }
    }

    /// Set the eviction order for this layout.
    pub fn with_eviction_order(mut self, order: Vec<String>) -> Self {
        self.eviction_order = order;
        self
    }

    /// Validate that the layout is well-formed.
    ///
    /// Checks:
    /// - Sum of max_tokens doesn't exceed total_budget_tokens
    /// - All region names in eviction_order exist
    /// - No duplicate region names
    pub fn validate(&self) -> Result<(), String> {
        // Check for duplicate region names
        let mut names = std::collections::HashSet::new();
        for region in &self.regions {
            if !names.insert(region.name.as_str()) {
                return Err(format!("Duplicate region name: {}", region.name));
            }
        }

        // Check that eviction_order regions exist
        for name in &self.eviction_order {
            if !names.contains(name.as_str()) {
                return Err(format!(
                    "Eviction order references unknown region: {}",
                    name
                ));
            }
        }

        // Warn if sum of max tokens exceeds budget (not necessarily an error,
        // since not all regions will be full simultaneously)
        let total_max: usize = self.regions.iter().map(|r| r.max_tokens).sum();
        if total_max > self.total_budget_tokens {
            tracing::warn!(
                "Sum of region max tokens ({}) exceeds total budget ({})",
                total_max,
                self.total_budget_tokens
            );
        }

        Ok(())
    }

    /// Get a region definition by name.
    pub fn get_region(&self, name: &str) -> Option<&RegionDefinition> {
        self.regions.iter().find(|r| r.name == name)
    }
}

/// Definition of a region in a layout.
///
/// This is the blueprint for creating a Region instance. It specifies the
/// region's configuration but doesn't contain actual content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegionDefinition {
    /// Unique name for this region
    pub name: String,

    /// Region lifecycle policy
    pub kind: RegionKind,

    /// Maximum tokens for this region
    pub max_tokens: usize,

    /// Optional validation schema
    pub schema: Option<RegionSchema>,

    /// Human-readable description of this region's purpose
    pub description: Option<String>,
}

impl RegionDefinition {
    /// Create a new region definition.
    pub fn new(name: String, kind: RegionKind, max_tokens: usize) -> Self {
        Self {
            name,
            kind,
            max_tokens,
            schema: None,
            description: None,
        }
    }

    /// Add a schema to this region definition.
    pub fn with_schema(mut self, schema: RegionSchema) -> Self {
        self.schema = Some(schema);
        self
    }

    /// Add a description to this region definition.
    pub fn with_description(mut self, description: String) -> Self {
        self.description = Some(description);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_layout_creation() {
        let regions = vec![
            RegionDefinition::new("pinned".to_string(), RegionKind::Pinned, 5000),
            RegionDefinition::new("temp".to_string(), RegionKind::Temporary, 10000),
        ];
        let layout = ContextLayout::new(regions, 20000);
        assert_eq!(layout.regions.len(), 2);
        assert_eq!(layout.total_budget_tokens, 20000);
    }

    #[test]
    fn test_layout_validation() {
        let regions = vec![RegionDefinition::new(
            "test".to_string(),
            RegionKind::Pinned,
            5000,
        )];
        let layout =
            ContextLayout::new(regions, 10000).with_eviction_order(vec!["test".to_string()]);

        assert!(layout.validate().is_ok());
    }

    #[test]
    fn test_duplicate_region_names() {
        let regions = vec![
            RegionDefinition::new("test".to_string(), RegionKind::Pinned, 5000),
            RegionDefinition::new("test".to_string(), RegionKind::Temporary, 3000),
        ];
        let layout = ContextLayout::new(regions, 10000);

        assert!(layout.validate().is_err());
    }
}
