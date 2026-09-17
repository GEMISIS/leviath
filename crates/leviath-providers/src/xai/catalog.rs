//! What xAI's own listings say about its models, and what this build knows
//! when they cannot be read.
//!
//! Measured against the live API (2026-09-16), with an API key and with a
//! Grok subscription's sign-in alike:
//!
//! - `GET /v1/models` carries every model with `id`, `aliases`,
//!   `context_length`, `created` and prices, including the long-context tier
//!   (`*_long_context` beside `long_context_threshold`).
//! - `GET /v1/language-models` carries the chat models again with
//!   `input_modalities` and `output_modalities`, and no `context_length`.
//!   Its modalities never name documents, yet every chat model reads a PDF,
//!   inline or by file id (grok-4.3, 2026-09-17), so a PDF is added to each.
//! - `GET /v1/image-generation-models` and `/v1/video-generation-models` carry
//!   the media models; an image model quotes `image_price`, a video model
//!   quotes nothing.
//!
//! **Prices are USD cents per 100 million tokens.** `20000` is $2.00 per
//! million, so every token price is divided by 10 000. `image_price` is in the
//! same tick unit the usage block reports costs in: 10^10 per dollar.

use std::collections::HashMap;

use serde_json::Value;

use crate::capabilities::{LimitsSource, Match, ModelCapabilities, Row};
use crate::learned::LearnedModel;
use crate::pricing::{ModelPricing, PriceTier, PriceUnit, UnitPrice};

/// Token prices arrive in cents per 100 million tokens; this is how many of
/// those make one dollar per million.
const CENTS_PER_100M_PER_DOLLAR_PER_M: f64 = 10_000.0;

/// The chat models named when the listing cannot be read, as
/// `(id, display name)`.
pub(crate) const CATALOG: &[(&str, &str)] = &[
    ("grok-4.6", "Grok 4.6"),
    ("grok-4.5", "Grok 4.5"),
    ("grok-4.3", "Grok 4.3"),
    ("grok-4.20-0309-reasoning", "Grok 4.20 Reasoning"),
    ("grok-4.20-0309-non-reasoning", "Grok 4.20"),
    ("grok-4.20-multi-agent-0309", "Grok 4.20 Multi-Agent"),
    ("grok-build-0.1", "Grok Build 0.1"),
];

/// What this build knows about the chat models, most specific first.
///
/// Windows are xAI's published context lengths. xAI publishes no output
/// ceiling, so the output figure is a conservative one: a stage that asks for
/// more is refused by the API with a message saying so, where one sized too
/// low would silently truncate.
pub(crate) const MODELS: &[Row] = &[
    Row {
        matches: &[Match::Prefix("grok-4.6"), Match::Prefix("grok-4.5")],
        temperature: true,
        tools: true,
        context: 500_000,
        output: 128_000,
    },
    Row {
        // Measured: no tool calling on the multi-agent model.
        matches: &[Match::Contains("multi-agent")],
        temperature: true,
        tools: false,
        context: 1_000_000,
        output: 128_000,
    },
    Row {
        matches: &[Match::Prefix("grok-4.3"), Match::Prefix("grok-4.20")],
        temperature: true,
        tools: true,
        context: 1_000_000,
        output: 128_000,
    },
    Row {
        matches: &[Match::Prefix("grok-build")],
        temperature: true,
        tools: true,
        context: 256_000,
        output: 64_000,
    },
];

/// The answer for a chat model this build does not name.
pub(crate) const FALLBACK_CAPABILITIES: ModelCapabilities = ModelCapabilities {
    supports_temperature: true,
    supports_streaming: true,
    supports_tools: true,
    supports_system_prompt: true,
    max_context_tokens: 256_000,
    max_output_tokens: 32_000,
    limits_source: LimitsSource::Builtin,
};

/// The table's answer for `model`.
pub(crate) fn table_capabilities(model: &str) -> ModelCapabilities {
    crate::capabilities::lookup(MODELS, model, FALLBACK_CAPABILITIES)
}

/// Whether `model` takes a `reasoning.effort`.
///
/// Not every Grok does: the 4.20 reasoning and non-reasoning models and Grok
/// Build choose their own depth, and xAI's catalogue lists no effort for them.
/// A model named here that refuses one anyway is remembered by the provider
/// and asked without it after that.
pub(crate) fn takes_effort(model: &str) -> bool {
    ["grok-4.6", "grok-4.5", "grok-4.3", "grok-4.20-multi-agent"]
        .iter()
        .any(|prefix| model.starts_with(prefix))
}

/// A token price in dollars per million, from cents per 100 million.
fn per_million(entry: &Value, key: &str) -> Option<f64> {
    entry
        .get(key)
        .and_then(Value::as_f64)
        .map(|cents| cents / CENTS_PER_100M_PER_DOLLAR_PER_M)
}

/// The rates an entry quotes, tier included, or `None` when it does not quote
/// both input and output.
fn token_pricing(entry: &Value) -> Option<ModelPricing> {
    let input = per_million(entry, "prompt_text_token_price")?;
    let output = per_million(entry, "completion_text_token_price")?;
    let cached = per_million(entry, "cached_prompt_text_token_price").unwrap_or(input);
    let threshold = entry
        .get("long_context_threshold")
        .and_then(Value::as_u64)
        .filter(|t| *t > 0);
    let long_context = threshold.and_then(|threshold| {
        let tier_input =
            per_million(entry, "prompt_text_token_price_long_context").filter(|p| *p > 0.0)?;
        Some(PriceTier {
            threshold_tokens: threshold as usize,
            input_per_mtok: tier_input,
            cached_input_per_mtok: per_million(
                entry,
                "cached_prompt_text_token_price_long_context",
            )
            .filter(|p| *p > 0.0)
            .unwrap_or(cached),
            // xAI bills no separate cache write.
            cache_write_per_mtok: tier_input,
            output_per_mtok: per_million(entry, "completion_text_token_price_long_context")
                .filter(|p| *p > 0.0)
                .unwrap_or(output),
        })
    });
    Some(ModelPricing {
        cached_input_per_mtok: cached,
        long_context,
        ..ModelPricing::flat(input, output)
    })
}

/// The listing's modality words as mime patterns.
fn modalities(entry: &Value, key: &str) -> Option<Vec<String>> {
    let words = entry.get(key).and_then(Value::as_array)?;
    Some(
        words
            .iter()
            .filter_map(Value::as_str)
            .filter_map(crate::mime_tables::modality_pattern)
            .map(str::to_string)
            .collect(),
    )
}

/// Every id an entry answers to beside its own.
fn aliases(entry: &Value) -> Vec<String> {
    entry
        .get("aliases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// What one xAI listing read teaches: the models by canonical id, and every
/// alias pointing at its canonical id.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Listing {
    /// Each model, by the id xAI calls canonical.
    pub(crate) models: HashMap<String, LearnedModel>,
    /// Alias to canonical id.
    pub(crate) aliases: HashMap<String, String>,
}

impl Listing {
    /// Record `id` with its aliases.
    fn insert(&mut self, id: String, entry: &Value, model: LearnedModel) {
        for alias in aliases(entry) {
            self.aliases.insert(alias, id.clone());
        }
        self.models.insert(id, model);
    }
}

/// Read `GET /v1/models` (a `data` array): every model's window, prices and
/// release date. A model that quotes an image price is a media model and is
/// left to [`read_media`]; one with no completion price is not a chat model.
pub(crate) fn read_models(body: &Value, listing: &mut Listing) -> usize {
    let entries = body.get("data").and_then(Value::as_array);
    let mut read = 0;
    for entry in entries.into_iter().flatten() {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        if entry.get("image_price").is_some() || entry.get("completion_text_token_price").is_none()
        {
            continue;
        }
        let model = LearnedModel {
            max_context_tokens: entry
                .get("context_length")
                .and_then(Value::as_u64)
                .map(|n| n as usize),
            pricing: token_pricing(entry),
            released: entry.get("created").and_then(Value::as_i64),
            ..LearnedModel::default()
        };
        listing.insert(id.to_string(), entry, model);
        read += 1;
    }
    read
}

/// Fold `GET /v1/language-models` (a `models` array) into a listing: what each
/// chat model takes and hands back. A model the first read did not carry is
/// added, with no window, so the compiled table sizes it.
pub(crate) fn read_modalities(body: &Value, listing: &mut Listing) {
    let entries = body.get("models").and_then(Value::as_array);
    for entry in entries.into_iter().flatten() {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        let mut model = listing
            .models
            .get(id)
            .cloned()
            .unwrap_or_else(|| LearnedModel {
                pricing: token_pricing(entry),
                released: entry.get("created").and_then(Value::as_i64),
                ..LearnedModel::default()
            });
        model.input_types = modalities(entry, "input_modalities").map(|mut types| {
            types.push("application/pdf".to_string());
            types
        });
        model.output_types = modalities(entry, "output_modalities");
        listing.insert(id.to_string(), entry, model);
    }
}

/// Fold an image- or video-generation listing (a `models` array) into a
/// listing. An image model's `image_price` is in ticks (10^10 per dollar), and
/// a per-quality table, when there is one, prices its dearest row: a stage's
/// quality is its own choice, and the listing price should not understate it.
pub(crate) fn read_media(body: &Value, listing: &mut Listing) {
    let entries = body.get("models").and_then(Value::as_array);
    for entry in entries.into_iter().flatten() {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        let ticks = entry
            .get("pricing")
            .and_then(Value::as_array)
            .and_then(|rows| {
                rows.iter()
                    .filter_map(|r| r.get("price_per_image").and_then(Value::as_f64))
                    .reduce(f64::max)
            })
            .or_else(|| entry.get("image_price").and_then(Value::as_f64));
        let model = LearnedModel {
            released: entry.get("created").and_then(Value::as_i64),
            // A media model takes no temperature and calls no tools.
            supports_temperature: Some(false),
            supports_tools: Some(false),
            pricing: ticks.map(|t| {
                ModelPricing::per_unit(UnitPrice {
                    usd: t / crate::responses::TICKS_PER_USD,
                    unit: PriceUnit::Image,
                })
            }),
            input_types: modalities(entry, "input_modalities"),
            output_types: modalities(entry, "output_modalities"),
            ..LearnedModel::default()
        };
        listing.insert(id.to_string(), entry, model);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn close(got: f64, want: f64) {
        assert!((got - want).abs() < 1e-9, "{got} != {want}");
    }

    /// The shape measured on the live `/v1/models`, trimmed.
    fn models_body() -> Value {
        json!({ "data": [
            {
                "id": "grok-4.20-0309-reasoning",
                "aliases": ["grok-4.20", "grok-4.20-reasoning-latest"],
                "context_length": 1000000,
                "created": 1773014400,
                "prompt_text_token_price": 12500,
                "cached_prompt_text_token_price": 2000,
                "completion_text_token_price": 25000,
                "prompt_text_token_price_long_context": 25000,
                "cached_prompt_text_token_price_long_context": 4000,
                "completion_text_token_price_long_context": 50000,
                "long_context_threshold": 200000
            },
            {
                "id": "latest",
                "aliases": [],
                "context_length": 131072,
                "prompt_text_token_price": 12500,
                "completion_text_token_price": 25000,
                "long_context_threshold": 0
            },
            { "id": "grok-imagine-image", "image_price": 200000000 },
            { "id": "no-price" },
            { "aliases": ["nameless"] }
        ]})
    }

    #[test]
    fn prices_are_read_from_cents_per_hundred_million_tokens() {
        let mut listing = Listing::default();
        assert_eq!(read_models(&models_body(), &mut listing), 2);
        let grok = &listing.models["grok-4.20-0309-reasoning"];
        let p = grok.pricing.unwrap();
        close(p.input_per_mtok, 1.25);
        close(p.cached_input_per_mtok, 0.2);
        close(p.output_per_mtok, 2.5);
        let tier = p.long_context.expect("a tier");
        assert_eq!(tier.threshold_tokens, 200_000);
        close(tier.input_per_mtok, 2.5);
        close(tier.cached_input_per_mtok, 0.4);
        close(tier.output_per_mtok, 5.0);
        assert_eq!(grok.max_context_tokens, Some(1_000_000));
        assert_eq!(grok.released, Some(1_773_014_400));
        // A zero threshold is no tier, and a missing cache price is the input.
        let latest = listing.models["latest"].pricing.unwrap();
        assert!(latest.long_context.is_none());
        close(latest.cached_input_per_mtok, 1.25);
        // Media and unpriced rows are not chat models.
        assert!(!listing.models.contains_key("grok-imagine-image"));
        assert!(!listing.models.contains_key("no-price"));
    }

    #[test]
    fn an_alias_resolves_to_its_canonical_model() {
        let mut listing = Listing::default();
        read_models(&models_body(), &mut listing);
        assert_eq!(
            listing.aliases.get("grok-4.20").map(String::as_str),
            Some("grok-4.20-0309-reasoning")
        );
        assert!(listing.models.contains_key("latest"));
        assert_eq!(listing.aliases.get("grok-9"), None);
    }

    #[test]
    fn a_tier_with_only_some_long_context_prices_falls_back_per_side() {
        let entry = json!({
            "prompt_text_token_price": 10000,
            "completion_text_token_price": 30000,
            "prompt_text_token_price_long_context": 20000,
            "long_context_threshold": 128000
        });
        let tier = token_pricing(&entry).unwrap().long_context.unwrap();
        close(tier.cached_input_per_mtok, 1.0);
        close(tier.output_per_mtok, 3.0);
        let no_tier_price = json!({
            "prompt_text_token_price": 10000,
            "completion_text_token_price": 30000,
            "long_context_threshold": 128000
        });
        assert!(
            token_pricing(&no_tier_price)
                .unwrap()
                .long_context
                .is_none()
        );
    }

    #[test]
    fn modalities_fold_into_the_models_already_read() {
        let mut listing = Listing::default();
        read_models(&models_body(), &mut listing);
        read_modalities(
            &json!({ "models": [
                { "id": "grok-4.20-0309-reasoning", "input_modalities": ["text", "image"], "output_modalities": ["text"] },
                { "id": "grok-new", "prompt_text_token_price": 20000, "completion_text_token_price": 60000,
                  "input_modalities": ["text", "sparkles"], "aliases": ["grok-newest"] },
                { "input_modalities": ["text"] }
            ]}),
            &mut listing,
        );
        let grok = &listing.models["grok-4.20-0309-reasoning"];
        assert_eq!(
            grok.input_types.as_deref(),
            Some(
                &[
                    "text/*".to_string(),
                    "image/*".to_string(),
                    "application/pdf".to_string()
                ][..]
            ),
            "a chat model reads PDFs, which the listing never names"
        );
        assert_eq!(
            grok.max_context_tokens,
            Some(1_000_000),
            "the window survived the fold"
        );
        let new = &listing.models["grok-new"];
        assert_eq!(new.max_context_tokens, None);
        close(new.pricing.unwrap().input_per_mtok, 2.0);
        assert_eq!(
            new.input_types.as_deref(),
            Some(&["text/*".to_string(), "application/pdf".to_string()][..])
        );
        assert_eq!(
            listing.aliases.get("grok-newest").map(String::as_str),
            Some("grok-new")
        );
    }

    #[test]
    fn media_listings_price_images_by_the_dearest_quality() {
        let mut listing = Listing::default();
        read_media(
            &json!({ "models": [
                { "id": "grok-imagine-image", "image_price": 200000000,
                  "input_modalities": ["text", "image"], "output_modalities": ["image"],
                  "aliases": ["grok-imagine-image-2026-03-02"] },
                { "id": "grok-imagine-image-2.0", "image_price": 600000000,
                  "pricing": [ { "price_per_image": 400000000 }, { "price_per_image": 800000000 } ] },
                { "id": "grok-imagine-video", "input_modalities": ["text", "image", "video"], "output_modalities": ["video"] },
                { "image_price": 1 }
            ]}),
            &mut listing,
        );
        let image = listing.models["grok-imagine-image"]
            .pricing
            .unwrap()
            .unit
            .unwrap();
        close(image.usd, 0.02);
        assert_eq!(image.unit, PriceUnit::Image);
        close(
            listing.models["grok-imagine-image-2.0"]
                .pricing
                .unwrap()
                .unit
                .unwrap()
                .usd,
            0.08,
        );
        let video = &listing.models["grok-imagine-video"];
        assert!(video.pricing.is_none());
        assert_eq!(video.supports_tools, Some(false));
        assert_eq!(
            video.output_types.as_deref(),
            Some(&["video/*".to_string()][..])
        );
        assert_eq!(
            listing
                .aliases
                .get("grok-imagine-image-2026-03-02")
                .map(String::as_str),
            Some("grok-imagine-image")
        );
    }

    #[test]
    fn an_empty_or_foreign_body_teaches_nothing() {
        let mut listing = Listing::default();
        assert_eq!(read_models(&json!({}), &mut listing), 0);
        read_modalities(&json!({ "data": [] }), &mut listing);
        read_media(&json!("nope"), &mut listing);
        assert_eq!(listing, Listing::default());
    }

    #[test]
    fn the_table_sizes_each_family_and_effort_is_per_model() {
        assert_eq!(table_capabilities("grok-4.6").max_context_tokens, 500_000);
        assert_eq!(table_capabilities("grok-4.3").max_context_tokens, 1_000_000);
        assert!(!table_capabilities("grok-4.20-multi-agent-0309").supports_tools);
        assert_eq!(
            table_capabilities("grok-build-0.1").max_output_tokens,
            64_000
        );
        assert_eq!(table_capabilities("grok-9"), FALLBACK_CAPABILITIES);
        assert!(takes_effort("grok-4.6"));
        assert!(takes_effort("grok-4.20-multi-agent-0309"));
        assert!(!takes_effort("grok-4.20-0309-reasoning"));
        assert!(!takes_effort("grok-build-0.1"));
        for (id, _) in CATALOG {
            assert_ne!(
                table_capabilities(id),
                FALLBACK_CAPABILITIES,
                "{id} has no row"
            );
        }
    }
}
