//! LLM compaction configuration for regions that summarize on overflow.

use serde::{Deserialize, Serialize};

/// Configuration for LLM-based compaction.
///
/// When a Compacting region hits its threshold, it sends content to an LLM
/// for summarization. This config controls which LLM is used and how
/// the summarization prompt is constructed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Provider to use for compaction (e.g., "anthropic", "openai")
    pub provider: String,

    /// Model to use (e.g., "claude-sonnet-4-6", "gpt-5.4-mini")
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
            model: "claude-sonnet-4-6".to_string(),
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
