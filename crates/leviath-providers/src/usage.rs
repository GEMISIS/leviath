//! Reading a reply's `usage` object into a [`TokenUsage`], for the shapes
//! more than one provider answers in.
//!
//! One reader per shape, so a buffered reply and a stream of the same call
//! cannot report it two ways, and so everything a reply says about how it is
//! billed (the amount charged, the service tier, the model that answered,
//! the kinds of token with a rate of their own) is read in one place.

use leviath_core::cost::CostUnknown;
use serde_json::Value;

use crate::pricing::TokenUsage;

/// The count at `path` in `usage`, zero when it is absent.
pub(crate) fn count(usage: &Value, path: &str) -> usize {
    usage.pointer(path).and_then(Value::as_u64).unwrap_or(0) as usize
}

/// A reply's `usage` in the OpenAI Chat Completions shape.
///
/// `prompt_tokens` there includes its `prompt_tokens_details` breakdown,
/// where [`TokenUsage::prompt_tokens`] is fresh input only, so the cache
/// counts come back out of it. Saturating: a gateway whose details exceed its
/// own total is malformed, and a zero is better than a wrapped figure.
///
/// The counts are the call's running total: a gateway that sends usage with
/// every chunk repeats it, so a later one replaces this one.
///
/// Audio and image tokens are billed at rates of their own, so a reply that
/// counts any names them as terms the token rates do not cover.
///
/// `cost` is what a gateway says it charged (OpenRouter, in US dollars). On a
/// call made with the account's own upstream key that is the gateway's fee
/// alone, and the upstream's charge is beside it in `cost_details`: the two
/// together are what the call cost, and a reply that names the fee without
/// the upstream's charge states no whole figure.
pub(crate) fn openai_chat(usage: &Value) -> TokenUsage {
    let cached = count(usage, "/prompt_tokens_details/cached_tokens");
    let written = count(usage, "/prompt_tokens_details/cache_write_tokens");
    let mut parsed = TokenUsage::new(
        count(usage, "/prompt_tokens")
            .saturating_sub(cached)
            .saturating_sub(written),
        cached,
        written,
        count(usage, "/completion_tokens"),
    )
    .whole();
    for (path, term) in [
        ("/prompt_tokens_details/audio_tokens", "audio_tokens"),
        ("/completion_tokens_details/audio_tokens", "audio_tokens"),
        ("/prompt_tokens_details/image_tokens", "image_tokens"),
        ("/completion_tokens_details/image_tokens", "image_tokens"),
    ] {
        if count(usage, path) > 0 && !parsed.terms.extra_terms.iter().any(|t| t == term) {
            parsed.terms.extra_terms.push(term.to_string());
        }
    }
    let charged = usage.get("cost").and_then(Value::as_f64);
    let own_key = usage.get("is_byok").and_then(Value::as_bool) == Some(true);
    let upstream = usage
        .pointer("/cost_details/upstream_inference_cost")
        .and_then(Value::as_f64);
    match (charged, own_key, upstream) {
        (Some(fee), true, Some(upstream)) => parsed.with_reported_cost(Some(fee + upstream)),
        (Some(_), true, None) => parsed.with_unknown_cost(CostUnknown::UnmodelledTerms),
        (charged, _, _) => parsed.with_reported_cost(charged),
    }
}

/// `usage` with what the reply's envelope says of the call: the model that
/// answered and the service tier it was served on.
pub(crate) fn with_envelope(mut usage: TokenUsage, reply: &Value) -> TokenUsage {
    let word = |key: &str| {
        reply
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if let Some(model) = word("model") {
        usage.terms.model = Some(model);
    }
    if let Some(tier) = word("service_tier") {
        usage.terms.service_tier = Some(tier);
    }
    usage
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::cost::CallCost;
    use serde_json::json;

    #[test]
    fn the_cache_counts_come_out_of_the_prompt_count() {
        let usage = openai_chat(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 7,
            "prompt_tokens_details": { "cached_tokens": 30, "cache_write_tokens": 20 }
        }));
        assert_eq!(
            (
                usage.prompt_tokens,
                usage.cached_tokens,
                usage.cache_write_tokens,
                usage.completion_tokens
            ),
            (50, 30, 20, 7)
        );
        assert!(usage.terms.whole, "a running total, not a slice");
        assert_eq!(usage.reported_cost_usd, None);
    }

    #[test]
    fn details_larger_than_the_total_clamp_to_zero() {
        let usage = openai_chat(&json!({
            "prompt_tokens": 5,
            "prompt_tokens_details": { "cached_tokens": 9 }
        }));
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.cached_tokens, 9);
    }

    #[test]
    fn audio_and_image_tokens_are_named_once_each() {
        let usage = openai_chat(&json!({
            "prompt_tokens": 10,
            "completion_tokens": 4,
            "prompt_tokens_details": { "audio_tokens": 3, "image_tokens": 2 },
            "completion_tokens_details": { "audio_tokens": 1, "image_tokens": 0 }
        }));
        assert_eq!(usage.terms.extra_terms, ["audio_tokens", "image_tokens"]);
    }

    #[test]
    fn a_gateways_charge_is_the_reported_cost() {
        let usage = openai_chat(&json!({ "prompt_tokens": 1, "cost": 0.004 }));
        assert_eq!(usage.reported_cost_usd, Some(0.004));
        // Said not to be on the account's own key: still the whole charge.
        let usage = openai_chat(&json!({ "prompt_tokens": 1, "cost": 0.004, "is_byok": false }));
        assert_eq!(usage.reported_cost_usd, Some(0.004));
    }

    #[test]
    fn on_the_accounts_own_key_the_fee_and_the_upstream_charge_add() {
        let usage = openai_chat(&json!({
            "prompt_tokens": 1,
            "cost": 0.001,
            "is_byok": true,
            "cost_details": { "upstream_inference_cost": 0.02 }
        }));
        assert!((usage.reported_cost_usd.unwrap() - 0.021).abs() < 1e-12);
    }

    #[test]
    fn a_fee_with_no_upstream_charge_beside_it_is_no_whole_figure() {
        let usage = openai_chat(&json!({
            "prompt_tokens": 1,
            "cost": 0.001,
            "is_byok": true,
            "cost_details": { "upstream_inference_cost": null }
        }));
        assert_eq!(usage.reported_cost_usd, None);
        assert_eq!(
            usage.cost(None),
            CallCost::Unknown(CostUnknown::UnmodelledTerms)
        );
    }

    #[test]
    fn the_envelope_names_the_model_and_the_tier() {
        let usage = with_envelope(
            TokenUsage::new(1, 0, 0, 1),
            &json!({ "model": "gpt-5.5-2026-04-23", "service_tier": "flex" }),
        );
        assert_eq!(usage.terms.model.as_deref(), Some("gpt-5.5-2026-04-23"));
        assert_eq!(usage.terms.service_tier.as_deref(), Some("flex"));
        // An envelope that names neither changes nothing.
        let plain = with_envelope(
            TokenUsage::new(1, 0, 0, 1),
            &json!({ "model": "", "service_tier": null }),
        );
        assert_eq!(plain.terms, crate::pricing::CallTerms::default());
    }
}
