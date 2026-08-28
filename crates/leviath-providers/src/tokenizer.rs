//! Token counting utilities for different LLM providers.
//!
//! Uses tiktoken-rs for accurate OpenAI model token counting and approximate
//! counting for other providers.

use tiktoken_rs::CoreBPE;

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
fn count_tokens_tiktoken(text: &str, model: &str) -> usize {
    bpe_for(model).encode_with_special_tokens(text).len()
}

/// The encoding a model this module can be asked about uses.
///
/// tiktoken-rs's own `bpe_for_model` knows every OpenAI model there has ever
/// been, and referencing it links every vocabulary it might answer with:
/// `r50k_base`, `p50k_base` and `p50k_edit` for the 2020-era completion
/// models, each a megabyte-plus table compiled into the binary. Nothing here
/// can reach them: [`count_tokens`] only asks for a name starting `gpt-`,
/// `o3-` or `o4-`, and every such name tiktoken's table resolves lands on
/// `o200k_base`, `o200k_harmony` or `cl100k_base`. This is that reachable
/// slice of the table, in the same order and with the same first-match
/// rules, so the three encodings are the only ones the binary carries.
///
/// The one name this answers differently: `gpt-2`, which tiktoken maps to
/// its GPT-2 encoding and this maps to `cl100k_base` like any other name it
/// does not know. Nobody runs an agent on GPT-2, and the count is a fallback
/// estimate either way. An unknown name falls back to `cl100k_base` exactly
/// as before.
fn bpe_for(model: &str) -> &'static CoreBPE {
    if model.starts_with("gpt-oss-") {
        return tiktoken_rs::o200k_harmony_singleton();
    }
    let o200k = model == "gpt-5"
        || model.starts_with("gpt-5-")
        || model.starts_with("gpt-5.")
        || model == "gpt-4.1"
        || model.starts_with("gpt-4.1-")
        || model.starts_with("gpt-4.5-")
        || model == "gpt-4o"
        || model.starts_with("gpt-4o-")
        || model.starts_with("o3-")
        || model == "o4-mini"
        || model.starts_with("o4-mini-");
    match o200k {
        true => tiktoken_rs::o200k_base_singleton(),
        false => tiktoken_rs::cl100k_base_singleton(),
    }
}

/// Approximate token count for Anthropic models (~3.5 chars per token).
fn approximate_count_anthropic(text: &str) -> usize {
    (text.len() as f32 / 3.5).ceil() as usize
}

/// Approximate token count for Gemini models (~4 chars per token).
fn approximate_count_gemini(text: &str) -> usize {
    leviath_core::estimate_tokens(text)
}

/// Approximate token count based on character length.
///
/// Uses the common heuristic of ~4 characters per token, which is reasonably
/// accurate for English text with GPT-style tokenizers.
pub fn approximate_count(text: &str) -> usize {
    leviath_core::estimate_tokens(text)
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
        let count = count_tokens("Hello, world!", "gpt-4");
        assert!(count > 0);
    }

    #[test]
    fn an_unrecognized_gpt_model_falls_back_to_cl100k() {
        assert!(std::ptr::eq(
            bpe_for("gpt-zzz"),
            tiktoken_rs::cl100k_base_singleton()
        ));
        let count = count_tokens("Hello, world!", "gpt-zzz");
        assert!(count > 0, "an unknown gpt-* model must still count tokens");
    }

    /// Every name this module can be asked about picks the encoding
    /// tiktoken-rs's own table would have picked, `gpt-2` excepted. The
    /// comparison is by pointer: the singletons are the encodings.
    ///
    /// The point of `bpe_for` is what it does NOT reference, so the test
    /// binary is where tiktoken's full table gets to be consulted.
    #[test]
    fn bpe_for_agrees_with_tiktokens_table_for_every_reachable_name() {
        let names = [
            "gpt-5",
            "gpt-5-mini",
            "gpt-5.5",
            "gpt-5.4-nano",
            "gpt-55",
            "gpt-4.1",
            "gpt-4.1-mini",
            "gpt-4.10",
            "gpt-4.5-preview",
            "gpt-4.5",
            "gpt-4o",
            "gpt-4o-2024-05-13",
            "gpt-4ox",
            "gpt-4",
            "gpt-4-0314",
            "gpt-4-32k",
            "gpt-3.5-turbo",
            "gpt-3.5",
            "gpt-3.5-turbo-0301",
            "gpt-35-turbo",
            "gpt-35-turbo-16k",
            "gpt-oss-120b",
            "gpt-oss",
            "gpt-zzz",
            "gpt-",
            "o3-",
            "o3-mini",
            "o3-pro",
            "o4-mini",
            "o4-mini-2026-06-02",
            "o4-",
            "o4-pro",
        ];
        let cl100k = tiktoken_rs::cl100k_base_singleton();
        for name in names {
            let theirs = tiktoken_rs::bpe_for_model(name).unwrap_or(cl100k);
            assert!(std::ptr::eq(bpe_for(name), theirs), "{name}");
        }
        // The documented exception.
        assert!(std::ptr::eq(bpe_for("gpt-2"), cl100k));
        assert!(!std::ptr::eq(
            tiktoken_rs::bpe_for_model("gpt-2").unwrap(),
            cl100k
        ));
        // And the harmony encoding is its own singleton, not o200k_base.
        assert!(!std::ptr::eq(
            bpe_for("gpt-oss-20b"),
            tiktoken_rs::o200k_base_singleton()
        ));
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
