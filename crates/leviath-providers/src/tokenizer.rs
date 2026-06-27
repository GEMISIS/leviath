//! Token counting utilities.

/// Token counter for estimating or precisely counting tokens.
pub struct TokenCounter {
    /// Provider name
    provider: String,
}

impl TokenCounter {
    /// Create a new token counter for the given provider.
    pub fn new(provider: String) -> Self {
        Self { provider }
    }

    /// Count tokens in the given text.
    ///
    /// This should use provider-specific tokenizers when available
    /// (tiktoken for OpenAI, Claude's tokenizer for Anthropic).
    pub fn count(&self, text: &str, model: &str) -> usize {
        // TODO: Implement proper tokenization
        // For now, rough estimate: ~4 characters per token
        text.len() / 4
    }

    /// Estimate tokens for a structured message.
    pub fn count_message(&self, role: &str, content: &str) -> usize {
        // Add overhead for message formatting
        self.count(content, "") + 10
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_counting() {
        let counter = TokenCounter::new("anthropic".to_string());
        let tokens = counter.count("Hello, world!", "claude-3-5-sonnet");
        assert!(tokens > 0);
    }
}
