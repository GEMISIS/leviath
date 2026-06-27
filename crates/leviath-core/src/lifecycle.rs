//! Region lifecycle policies and eviction strategies.
//!
//! This module defines the policies that control how regions behave when
//! the context window fills up, including eviction strategies for different
//! region types and compaction algorithms.

use serde::{Deserialize, Serialize};

/// Policy for evicting content from regions when space is needed.
///
/// The eviction policy determines the order and strategy for removing content
/// from regions to make room for new content when the context window approaches
/// its token budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EvictionPolicy {
    /// Least Recently Used - evict oldest entries first
    LRU,

    /// First In First Out - evict in insertion order
    FIFO,

    /// Least Frequently Used - evict least accessed entries
    LFU,

    /// Custom policy with user-defined scoring
    Custom { scorer: String },
}

impl Default for EvictionPolicy {
    fn default() -> Self {
        Self::LRU
    }
}

/// Strategy for compacting (summarizing) region content.
///
/// Compaction reduces token usage while preserving essential information.
/// Different strategies make different trade-offs between compression ratio
/// and information retention.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CompactionStrategy {
    /// Use an LLM to summarize the content
    LLMSummarization {
        /// Target token count after summarization
        target_tokens: usize,
        /// Model to use for summarization
        model: Option<String>,
    },

    /// Extract key facts and discard narrative
    KeyFactExtraction,

    /// Keep only the most recent N items and summarize the rest
    RecencyBiased {
        /// Number of recent items to keep in full
        keep_recent: usize,
        /// Target tokens for summarized older content
        summary_tokens: usize,
    },

    /// Custom compaction logic
    Custom { function: String },
}

impl Default for CompactionStrategy {
    fn default() -> Self {
        Self::LLMSummarization {
            target_tokens: 2000,
            model: None,
        }
    }
}

/// Configuration for managing region lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleConfig {
    /// Default eviction policy for regions
    pub eviction_policy: EvictionPolicy,

    /// Default compaction strategy for Compacting regions
    pub compaction_strategy: CompactionStrategy,

    /// Token threshold for triggering eviction (as % of total budget)
    pub eviction_threshold: f32,

    /// Minimum free tokens to maintain after eviction
    pub min_free_tokens: usize,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            eviction_policy: EvictionPolicy::default(),
            compaction_strategy: CompactionStrategy::default(),
            eviction_threshold: 0.9, // Start eviction at 90% full
            min_free_tokens: 1000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_lifecycle_config() {
        let config = LifecycleConfig::default();
        assert_eq!(config.eviction_threshold, 0.9);
        assert_eq!(config.min_free_tokens, 1000);
    }
}
