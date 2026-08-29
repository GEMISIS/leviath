//! What OpenRouter's `GET /models` says about one model.
//!
//! Measured against the live endpoint (398 models, 2026-08-28) rather than
//! read off a page: every entry carries `id`, `name`, `created`,
//! `context_length`, `top_provider.max_completion_tokens`, a `pricing` object
//! quoting USD per token as strings, and a `supported_parameters` array
//! naming what the request may carry. Three entries had no
//! `supported_parameters` at all, so that field is optional here and the
//! compiled table answers for those.

use crate::learned::LearnedModel;
use crate::pricing::ModelPricing;

/// One model's rates from a `/models` entry, or `None` when it does not quote
/// both sides.
///
/// The endpoint reports USD **per token** as strings; `ModelPricing` is per
/// million, hence the scale. A model missing either half is skipped rather than
/// half-priced: a total built from a prompt rate and no completion rate is
/// wrong in a way that looks right.
///
/// Cache rates are quoted separately and often absent. When they are, the read
/// rate falls back to the input rate and the write rate with it - the same
/// shape as a provider that does not price caching separately, which is what
/// `ModelPricing::flat` encodes.
pub(crate) fn parse_pricing(entry: &serde_json::Value) -> Option<ModelPricing> {
    let p = entry.get("pricing")?;
    let rate = |key: &str| -> Option<f64> {
        p.get(key)
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .map(|per_token| per_token * 1_000_000.0)
    };
    let input = rate("prompt")?;
    let output = rate("completion")?;
    Some(ModelPricing {
        input_per_mtok: input,
        cached_input_per_mtok: rate("input_cache_read").unwrap_or(input),
        cache_write_per_mtok: rate("input_cache_write").unwrap_or(input),
        output_per_mtok: output,
    })
}

/// Whether the entry quotes a cache-write rate at all.
///
/// Read off the raw key rather than the parsed rates, because
/// [`parse_pricing`] backfills an absent write rate with the input rate and
/// the question here is whether the upstream bills one. A vendor that does
/// (Anthropic, Google, Qwen) reads explicit `cache_control` markers; one that
/// does not (DeepSeek, xAI, Moonshot, Mistral) caches by prefix on its own and
/// is sent plain text.
fn quotes_cache_write(entry: &serde_json::Value) -> bool {
    entry
        .get("pricing")
        .and_then(|p| p.get("input_cache_write"))
        .and_then(|v| v.as_str())
        .is_some()
}

/// The listing's entry as a [`LearnedModel`], keyed by its id.
///
/// Every entry with an id is recorded, even one with no `context_length`:
/// the catalogue is also the list of what the gateway serves, and a model
/// whose size the listing omits is still a model it routes to. Each field is
/// `None` when the entry lacks it, so the compiled table keeps that answer.
pub(crate) fn parse_entry(entry: &serde_json::Value) -> Option<(String, LearnedModel)> {
    let id = entry.get("id").and_then(|v| v.as_str())?.to_string();
    let size = |v: &serde_json::Value| v.as_u64().map(|n| n as usize);
    // `supported_parameters` names what the request may carry. Absent from
    // the list means the gateway will not forward it, which is the answer the
    // runtime wants: `openai/gpt-5.5` is listed without `temperature` and
    // refuses one with a 400. The rest of the gpt-5 line is listed without
    // one too and silently ignores it, so nothing is lost by not sending it.
    let params = entry.get("supported_parameters").and_then(|v| v.as_array());
    let takes = |name: &str| params.map(|p| p.iter().any(|v| v.as_str() == Some(name)));
    Some((
        id,
        LearnedModel {
            display_name: entry
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            max_context_tokens: entry.get("context_length").and_then(size),
            max_output_tokens: entry
                .get("top_provider")
                .and_then(|tp| tp.get("max_completion_tokens"))
                .and_then(size),
            supports_temperature: takes("temperature"),
            supports_tools: takes("tools"),
            explicit_cache_control: Some(quotes_cache_write(entry)),
            pricing: parse_pricing(entry),
            released: entry.get("created").and_then(|v| v.as_i64()),
            // The listing has no retirement date; OpenRouter simply drops a
            // model from it.
            retires: None,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_full_entry_fills_every_field_the_listing_has() {
        let entry = json!({
            "id": "qwen/qwen3.6-plus",
            "name": "Qwen3.6 Plus",
            "created": 1_775_133_557,
            "context_length": 1_000_000,
            "top_provider": { "max_completion_tokens": 65_536 },
            "pricing": {
                "prompt": "0.000001",
                "completion": "0.000004",
                "input_cache_write": "0.00000040625"
            },
            "supported_parameters": ["max_tokens", "temperature", "tools"]
        });
        let (id, learned) = parse_entry(&entry).unwrap();
        assert_eq!(id, "qwen/qwen3.6-plus");
        assert_eq!(learned.display_name.as_deref(), Some("Qwen3.6 Plus"));
        assert_eq!(learned.max_context_tokens, Some(1_000_000));
        assert_eq!(learned.max_output_tokens, Some(65_536));
        assert_eq!(learned.supports_temperature, Some(true));
        assert_eq!(learned.supports_tools, Some(true));
        assert_eq!(learned.explicit_cache_control, Some(true));
        assert_eq!(learned.released, Some(1_775_133_557));
        assert_eq!(learned.retires, None);
        let pricing = learned.pricing.unwrap();
        assert!((pricing.input_per_mtok - 1.0).abs() < 1e-9);
        assert!((pricing.output_per_mtok - 4.0).abs() < 1e-9);
        // No read rate quoted: it falls back to the input rate.
        assert!((pricing.cached_input_per_mtok - 1.0).abs() < 1e-9);
        assert!((pricing.cache_write_per_mtok - 0.40625).abs() < 1e-9);
    }

    #[test]
    fn a_model_listed_without_temperature_or_tools_is_recorded_as_refusing_them() {
        let entry = json!({
            "id": "openai/gpt-5.5",
            "supported_parameters": ["max_tokens", "reasoning"]
        });
        let (_, learned) = parse_entry(&entry).unwrap();
        assert_eq!(learned.supports_temperature, Some(false));
        assert_eq!(learned.supports_tools, Some(false));
    }

    #[test]
    fn an_entry_without_supported_parameters_leaves_the_flags_to_the_table() {
        let entry = json!({ "id": "sakana/sakana-namazu", "context_length": 32_000 });
        let (_, learned) = parse_entry(&entry).unwrap();
        assert_eq!(learned.supports_temperature, None);
        assert_eq!(learned.supports_tools, None);
        assert_eq!(learned.max_context_tokens, Some(32_000));
        assert_eq!(learned.max_output_tokens, None);
        assert_eq!(learned.display_name, None);
        assert_eq!(learned.released, None);
        assert_eq!(learned.pricing, None);
    }

    #[test]
    fn a_read_only_cache_price_means_no_markers() {
        let entry = json!({
            "id": "deepseek/deepseek-v4-pro",
            "pricing": { "prompt": "0.0000003", "completion": "0.0000012", "input_cache_read": "0.000000022" }
        });
        let (_, learned) = parse_entry(&entry).unwrap();
        assert_eq!(learned.explicit_cache_control, Some(false));
        let pricing = learned.pricing.unwrap();
        assert!((pricing.cached_input_per_mtok - 0.022).abs() < 1e-9);
        // The write rate is backfilled from input, which is why the marker
        // signal reads the raw key instead.
        assert!((pricing.cache_write_per_mtok - 0.3).abs() < 1e-9);
    }

    #[test]
    fn an_entry_without_an_id_is_dropped() {
        assert_eq!(parse_entry(&json!({ "name": "nameless" })), None);
    }

    #[test]
    fn half_a_price_is_no_price() {
        let entry = json!({ "id": "x", "pricing": { "prompt": "0.000001" } });
        assert_eq!(parse_entry(&entry).unwrap().1.pricing, None);
        let entry = json!({ "id": "x", "pricing": { "prompt": "abc", "completion": "0.1" } });
        assert_eq!(parse_entry(&entry).unwrap().1.pricing, None);
    }
}
