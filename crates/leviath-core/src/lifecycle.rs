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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum EvictionPolicy {
    /// Least Recently Used - evict oldest entries first
    #[default]
    LRU,

    /// First In First Out - evict in insertion order
    FIFO,

    /// Least Frequently Used - evict least accessed entries
    LFU,

    /// Custom policy with user-defined scoring
    Custom { scorer: String },
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

/// Configuration for LLM-based compaction.
///
/// When a Compacting region hits its threshold, it sends content to an LLM
/// for summarization. This config controls which LLM is used and how
/// the summarization prompt is constructed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Provider to use for compaction (e.g., "anthropic", "openai")
    pub provider: String,

    /// Model to use (e.g., "claude-sonnet-4", "gpt-4o-mini")
    pub model: String,

    /// Custom system prompt for compaction (None = use default)
    pub system_prompt: Option<String>,

    /// Custom user prompt template. Use {content} as placeholder for region
    /// content, {region_name} for the region name.
    pub user_prompt_template: Option<String>,

    /// Max tokens for the summary response
    pub max_summary_tokens: usize,

    /// Temperature (lower = more deterministic)
    pub temperature: f32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4".to_string(),
            system_prompt: None,
            user_prompt_template: None,
            max_summary_tokens: 2000,
            temperature: 0.2,
        }
    }
}

impl CompactionConfig {
    /// Get the system prompt, using the default if none is configured.
    pub fn system_prompt(&self) -> &str {
        self.system_prompt.as_deref().unwrap_or(
            "You are a context compaction assistant. Your job is to summarize content \
             concisely while preserving all key information, decisions, and actionable items. \
             Never lose critical details.",
        )
    }

    /// Get the user prompt, substituting {content} and {region_name} placeholders.
    pub fn user_prompt(&self, content: &str, region_name: &str) -> String {
        if let Some(template) = &self.user_prompt_template {
            template
                .replace("{content}", content)
                .replace("{region_name}", region_name)
        } else {
            format!(
                "Summarize the following content from the \"{}\" context region. \
                 Preserve key facts, decisions, code snippets, and actionable items. \
                 Be concise but thorough.\n\n{}",
                region_name, content
            )
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

    #[test]
    fn test_compaction_config_defaults() {
        let config = CompactionConfig::default();
        assert_eq!(config.max_summary_tokens, 2000);
        assert_eq!(config.temperature, 0.2);
        assert!(config.system_prompt().contains("compaction assistant"));
    }

    #[test]
    fn test_compaction_config_user_prompt() {
        let config = CompactionConfig::default();
        let prompt = config.user_prompt("some content here", "conversation");
        assert!(prompt.contains("conversation"));
        assert!(prompt.contains("some content here"));
    }

    #[test]
    fn test_compaction_config_custom_template() {
        let config = CompactionConfig {
            user_prompt_template: Some("Region {region_name}: {content}".to_string()),
            ..Default::default()
        };
        let prompt = config.user_prompt("hello", "test");
        assert_eq!(prompt, "Region test: hello");
    }
}
