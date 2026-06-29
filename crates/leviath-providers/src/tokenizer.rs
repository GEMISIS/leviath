//! Token counting utilities for different LLM providers.
//!
//! Uses tiktoken-rs for accurate OpenAI model token counting and approximate
//! counting for other providers.

use tiktoken_rs::get_bpe_from_model;

/// Count tokens in text for a specific model.
///
/// Uses tiktoken for OpenAI/GPT models, approximate counting for others.
pub fn count_tokens(text: &str, model: &str) -> usize {
    if model.starts_with("gpt-") || model.starts_with("o3-") || model.starts_with("o4-") {
        count_tokens_tiktoken(text, model)
    } else if model.starts_with("claude-") {
        // Anthropic: ~3.5 chars per token (no official Rust tokenizer)
        approximate_count_anthropic(text)
    } else {
        approximate_count(text)
    }
}

/// Count tokens using tiktoken for OpenAI models.
fn count_tokens_tiktoken(text: &str, model: &str) -> usize {
    match get_bpe_from_model(model) {
        Ok(bpe) => bpe.encode_with_special_tokens(text).len(),
        Err(_) => {
            // Fall back to cl100k_base (GPT-4 encoding) if model not recognized
            match tiktoken_rs::cl100k_base() {
                Ok(bpe) => bpe.encode_with_special_tokens(text).len(),
                Err(_) => approximate_count(text),
            }
        }
    }
}

/// Approximate token count for Anthropic models (~3.5 chars per token).
fn approximate_count_anthropic(text: &str) -> usize {
    (text.len() as f32 / 3.5).ceil() as usize
}

/// Approximate token count based on character length.
///
/// Uses the common heuristic of ~4 characters per token, which is reasonably
/// accurate for English text with GPT-style tokenizers.
pub fn approximate_count(text: &str) -> usize {
    text.len().div_ceil(4)
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
    fn test_tiktoken_count() {
        let text = "Hello, world! This is a test.";
        let count = count_tokens(text, "gpt-5.4-mini");
        // tiktoken should give a reasonable count (falls back to cl100k_base)
        assert!(count > 0);
        assert!(count < text.len());
    }

    #[test]
    fn test_anthropic_count() {
        let text = "Hello, world! This is a test.";
        let count = count_tokens(text, "claude-sonnet-4-6");
        assert!(count > 0);
        assert!(count < text.len());
    }
}
