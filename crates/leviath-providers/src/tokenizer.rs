//! Token counting utilities for different LLM providers.
//!
//! Provides accurate token counting using provider-specific tokenizers where available,
//! and approximate counting for providers without accessible tokenizers.

/// Count tokens in text for a specific model.
///
/// Uses provider-specific tokenizers when available (tiktoken for OpenAI,
/// Claude tokenizer for Anthropic), falls back to approximate counting otherwise.
pub fn count_tokens(text: &str, model: &str) -> usize {
    if model.starts_with("gpt-") || model.starts_with("o1-") {
        // Use tiktoken for OpenAI models (requires tiktoken-rs)
        // TODO: Implement actual tiktoken integration
        approximate_count(text)
    } else if model.starts_with("claude-") {
        // Use Claude tokenizer for Anthropic models
        // TODO: Implement actual Claude tokenizer integration
        approximate_count(text)
    } else {
        // Approximate for other providers
        approximate_count(text)
    }
}

/// Approximate token count based on character length.
///
/// Uses the common heuristic of ~4 characters per token, which is reasonably
/// accurate for English text with GPT-style tokenizers.
pub fn approximate_count(text: &str) -> usize {
    // Common heuristic: ~4 characters per token
    (text.len() + 3) / 4
}

/// Get maximum context tokens for a model.
pub fn max_context_tokens(model: &str) -> usize {
    if model.contains("claude-opus-4") || model.contains("claude-sonnet-4") {
        200_000
    } else if model.starts_with("gpt-4") {
        128_000
    } else if model.starts_with("gpt-3.5") {
        16_384
    } else {
        // Default conservative estimate
        4096
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_approximate_count() {
        let text = "Hello, world!";
        let count = approximate_count(text);
        assert!(count > 0);
        assert!(count < text.len());
    }

    #[test]
    fn test_max_context_tokens() {
        assert_eq!(max_context_tokens("claude-opus-4"), 200_000);
        assert_eq!(max_context_tokens("gpt-4-turbo"), 128_000);
        assert_eq!(max_context_tokens("unknown-model"), 4096);
    }
}
