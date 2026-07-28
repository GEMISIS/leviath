//! Token counting utilities for different LLM providers.
//!
//! Uses tiktoken-rs for accurate OpenAI model token counting and approximate
//! counting for other providers.

use tiktoken_rs::bpe_for_model;

/// Count tokens in text for a specific model.
///
/// Uses tiktoken for OpenAI/GPT models, approximate counting for others.
///
/// This is the offline/heuristic path. Providers with an exact remote
/// token-count endpoint (Anthropic, Gemini) call it only as the fallback when
/// that endpoint is unavailable; see each provider's `count_tokens`.
pub fn count_tokens(text: &str, model: &str) -> usize {
    if model.starts_with("gpt-") || model.starts_with("o3-") || model.starts_with("o4-") {
        count_tokens_tiktoken(text, model)
    } else if model.starts_with("claude-") {
        // Anthropic: ~3.5 chars per token (no official Rust tokenizer)
        approximate_count_anthropic(text)
    } else if model.starts_with("gemini-") {
        // Gemini: no official Rust tokenizer; ~4 chars per token (SentencePiece,
        // close to GPT for English). Kept as its own branch so the ratio can be
        // tuned independently of the generic fallback.
        approximate_count_gemini(text)
    } else {
        approximate_count(text)
    }
}

/// Count tokens using tiktoken for OpenAI models.
///
/// Both halves return `&'static CoreBPE` from tiktoken-rs's cached singletons,
/// so an unknown model falls back to `cl100k_base` without building a second
/// encoder — and without the `.expect()` the old fallible `cl100k_base()`
/// required, since the singleton accessor cannot fail.
fn count_tokens_tiktoken(text: &str, model: &str) -> usize {
    let bpe = bpe_for_model(model).unwrap_or_else(|_| tiktoken_rs::cl100k_base_singleton());
    bpe.encode_with_special_tokens(text).len()
}

/// Approximate token count for Anthropic models (~3.5 chars per token).
fn approximate_count_anthropic(text: &str) -> usize {
    (text.len() as f32 / 3.5).ceil() as usize
}

/// Approximate token count for Gemini models (~4 chars per token).
fn approximate_count_gemini(text: &str) -> usize {
    text.len().div_ceil(4)
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

    // ── Additional coverage tests ──────────────────────────────────────────

    #[test]
    fn test_empty_string() {
        assert_eq!(count_tokens("", "gpt-5.4-mini"), 0);
        assert_eq!(count_tokens("", "claude-sonnet-4-6"), 0);
        assert_eq!(count_tokens("", "unknown-model"), 0);
    }

    #[test]
    fn test_approximate_count_empty() {
        assert_eq!(approximate_count(""), 0);
    }

    #[test]
    fn test_approximate_count_exact_multiple() {
        // 8 chars / 4 = 2 tokens
        assert_eq!(approximate_count("12345678"), 2);
    }

    #[test]
    fn test_approximate_count_non_multiple() {
        // 9 chars / 4 = 2.25 → ceil = 3
        assert_eq!(approximate_count("123456789"), 3);
    }

    #[test]
    fn test_approximate_count_single_char() {
        assert_eq!(approximate_count("a"), 1);
    }

    #[test]
    fn test_gpt_model_uses_tiktoken() {
        let count = count_tokens("Hello", "gpt-5.5");
        assert!(count > 0);
    }

    #[test]
    fn test_recognized_gpt4_model_resolves_bpe_directly() {
        // "gpt-4" is recognized by tiktoken-rs's own model table, so this
        // exercises the `Ok(bpe)` branch of `get_bpe_from_model` directly,
        // as opposed to the cl100k_base fallback used for fictional/future
        // model names like "gpt-5.5" above.
        let count = count_tokens("Hello, world!", "gpt-4");
        assert!(count > 0);
    }

    #[test]
    fn test_o3_model_uses_tiktoken() {
        let count = count_tokens("Hello", "o3-mini");
        assert!(count > 0);
    }

    #[test]
    fn test_o4_model_uses_tiktoken() {
        let count = count_tokens("Hello", "o4-mini");
        assert!(count > 0);
    }

    #[test]
    fn test_claude_model_uses_anthropic_approx() {
        // "Hello" = 5 chars, 5/3.5 = 1.43, ceil = 2
        let count = count_tokens("Hello", "claude-sonnet-4-6");
        assert_eq!(count, 2);
    }

    #[test]
    fn test_gemini_model_uses_gemini_approx() {
        // "12345678" = 8 chars, 8/4 = 2 tokens (gemini branch, not tiktoken)
        assert_eq!(count_tokens("12345678", "gemini-3.5-flash"), 2);
        // 9 chars / 4 = 2.25 → ceil = 3
        assert_eq!(count_tokens("123456789", "gemini-3.1-pro-preview"), 3);
    }

    #[test]
    fn test_unknown_model_uses_general_approx() {
        // "Hello" = 5 chars, 5/4 = 1.25, ceil = 2
        let count = count_tokens("Hello", "unknown-model");
        assert_eq!(count, 2);
    }

    #[test]
    fn test_anthropic_approximation_rounding() {
        // 7 chars / 3.5 = 2.0 exactly
        let count = count_tokens("1234567", "claude-opus-4-8");
        assert_eq!(count, 2);
    }

    #[test]
    fn test_long_string() {
        let text = "a".repeat(10000);
        let gpt_count = count_tokens(&text, "gpt-5.4-mini");
        let claude_count = count_tokens(&text, "claude-sonnet-4-6");
        let unknown_count = count_tokens(&text, "unknown");
        assert!(gpt_count > 0);
        assert!(claude_count > 0);
        assert!(unknown_count > 0);
    }
}
