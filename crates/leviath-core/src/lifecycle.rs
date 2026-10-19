//! LLM compaction configuration for regions that summarize on overflow.

use serde::{Deserialize, Serialize};

use crate::names::ModelRef;

/// The largest summary a compaction call asks for when the graph says
/// nothing.
pub const DEFAULT_MAX_SUMMARY_TOKENS: usize = 2000;

/// The sampling temperature of a compaction call when the graph says nothing.
pub const DEFAULT_SUMMARY_TEMPERATURE: f32 = 0.2;

/// Configuration for LLM-based compaction.
///
/// When a Compacting region hits its threshold, it sends content to an LLM
/// for summarization. This config controls which LLM is used and how
/// the summarization prompt is constructed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Where a summary is sent, best first: each a provider and a model on
    /// it, resolved against the operator's provider preference when the run
    /// was resolved, so the routes after the first are its failover. Empty
    /// sends the summary to the model the current stage runs on, failing
    /// over along that stage's own fallbacks.
    pub routes: Vec<ModelRef>,

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
    /// A summary on the stage's own model, at the default length and
    /// temperature.
    fn default() -> Self {
        Self {
            routes: Vec::new(),
            system_prompt: None,
            user_prompt_template: None,
            max_summary_tokens: DEFAULT_MAX_SUMMARY_TOKENS,
            temperature: DEFAULT_SUMMARY_TEMPERATURE,
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
    fn test_compaction_config_defaults() {
        let config = CompactionConfig::default();
        assert!(config.routes.is_empty(), "the stage's own model");
        assert_eq!(config.max_summary_tokens, DEFAULT_MAX_SUMMARY_TOKENS);
        assert_eq!(config.temperature, DEFAULT_SUMMARY_TEMPERATURE);
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
