//! What Bedrock charges, read from AWS's public price list.
//!
//! AWS publishes every service's rates as a JSON offer file, one per region,
//! with no credentials needed. The Bedrock file names each model by its
//! display name rather than its id and prices each token kind as its own
//! product, so reading it is a matter of collecting the on-demand token
//! products per name and matching the names to what the listing said. That
//! is done once at priming and the result rides on the learned record, so
//! the accounting path never touches the network.
//!
//! One gap the file has: the Claude models newer than Claude 3 are billed
//! through AWS Marketplace and are not in it at all. Those are priced from
//! the Anthropic rate rows this crate ships, which are Bedrock's list price
//! for the same models; see `BedrockProvider::pricing`.

use std::collections::HashMap;

use serde_json::Value;

use super::catalog::{Vendor, vendor_model, vendor_of};
use crate::learned::LearnedModel;
use crate::pricing::ModelPricing;

/// The public offer file for Bedrock in `region`.
pub(super) fn price_file_url(region: &str) -> String {
    format!(
        "https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws/AmazonBedrock/current/{region}/index.json"
    )
}

/// The most of an offer file that is read: the regional file is a megabyte
/// or two, and a cap keeps a wrong URL from being read forever.
pub(super) const PRICE_FILE_CAP: usize = 32 * 1024 * 1024;

/// Words in a `usagetype` that mark a product as something other than the
/// standard on-demand rate.
const NOT_STANDARD: &[&str] = &[
    "batch",
    "latency-optimized",
    "custom-model",
    "priority",
    "flex",
];

/// The token products, by the `inferenceType` the file names them with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Input,
    Output,
    CacheRead,
    CacheWrite,
}

impl Kind {
    fn parse(inference_type: &str) -> Option<Self> {
        match inference_type {
            "Input tokens" => Some(Kind::Input),
            "Output tokens" => Some(Kind::Output),
            "Prompt cache read input tokens" => Some(Kind::CacheRead),
            "Prompt cache write input tokens" => Some(Kind::CacheWrite),
            _ => None,
        }
    }
}

/// The four rates of one model as they are found, USD per million tokens.
#[derive(Debug, Default, Clone, Copy)]
struct Partial {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

impl Partial {
    /// A rate card, once both the input and the output rate are known. A
    /// vendor that quotes no cache rate bills a read as input and a write
    /// at no extra cost, which is what the input rate in both slots means.
    fn pricing(self) -> Option<ModelPricing> {
        let input = self.input?;
        let output = self.output?;
        Some(ModelPricing {
            input_per_mtok: input,
            cached_input_per_mtok: self.cache_read.unwrap_or(input),
            cache_write_per_mtok: self.cache_write.filter(|w| *w > 0.0).unwrap_or(input),
            output_per_mtok: output,
            long_context: None,
            unit: None,
        })
    }
}

/// A name as a matching key: lowercase letters and digits only, so
/// `Llama 4 Maverick 17B`, `llama4-maverick-17b` and `Llama-4-Maverick-17B`
/// are one key.
pub(super) fn normalize(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// The standard on-demand token rates in an offer file, by normalized
/// display name.
pub(super) fn parse_price_file(file: &Value) -> HashMap<String, ModelPricing> {
    let products = file.get("products").and_then(|p| p.as_object());
    let terms = file.pointer("/terms/OnDemand").and_then(|t| t.as_object());
    let (Some(products), Some(terms)) = (products, terms) else {
        return HashMap::new();
    };
    let mut partials: HashMap<String, Partial> = HashMap::new();
    for (sku, product) in products {
        let attributes = product.get("attributes");
        let attr = |key: &str| attributes.and_then(|a| a.get(key)).and_then(|v| v.as_str());
        let Some(kind) = attr("inferenceType").and_then(Kind::parse) else {
            continue;
        };
        let Some(model) = attr("model") else {
            continue;
        };
        let standard = match attr("feature") {
            Some(feature) => feature == "On-demand Inference",
            None => attr("service_tier").is_none_or(|tier| tier == "standard"),
        };
        let usage = attr("usagetype").unwrap_or("").to_ascii_lowercase();
        if !standard || NOT_STANDARD.iter().any(|w| usage.contains(w)) {
            continue;
        }
        let Some(rate) = rate_per_mtok(terms.get(sku)) else {
            continue;
        };
        let entry = partials.entry(normalize(model)).or_default();
        let slot = match kind {
            Kind::Input => &mut entry.input,
            Kind::Output => &mut entry.output,
            Kind::CacheRead => &mut entry.cache_read,
            Kind::CacheWrite => &mut entry.cache_write,
        };
        // The same rate listed twice (once per endpoint) is one rate.
        slot.get_or_insert(rate);
    }
    partials
        .into_iter()
        .filter_map(|(name, partial)| partial.pricing().map(|p| (name, p)))
        .collect()
}

/// The USD rate of one SKU's on-demand term, per million tokens, when the
/// term is priced per thousand tokens.
fn rate_per_mtok(term: Option<&Value>) -> Option<f64> {
    let dimensions = term?
        .as_object()?
        .values()
        .next()?
        .get("priceDimensions")?
        .as_object()?;
    let dimension = dimensions.values().next()?;
    if dimension.get("unit").and_then(|u| u.as_str()) != Some("1K tokens") {
        return None;
    }
    let usd: f64 = dimension
        .pointer("/pricePerUnit/USD")?
        .as_str()?
        .trim()
        .parse()
        .ok()?;
    Some(usd * 1000.0)
}

/// The keys a learned model may be listed under in the price file: its
/// display name, and its id with the vendor and the version suffix removed.
fn listing_keys(id: &str, learned: &LearnedModel) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(name) = &learned.display_name {
        keys.push(normalize(name));
    }
    let tail = vendor_model(id);
    // `nova-pro-v1:0` and `gpt-oss-120b-1:0` carry a version the file's
    // names do not.
    let tail = tail
        .rsplit_once(':')
        .map_or(tail, |(before, _)| {
            before.trim_end_matches(|c: char| c.is_ascii_digit())
        })
        .trim_end_matches("-v")
        .trim_end_matches('-');
    let key = normalize(tail);
    if !keys.contains(&key) {
        keys.push(key);
    }
    keys
}

/// Attach a rate card to every learned model the file prices.
///
/// A model matches by an exact key first, and failing that by the one price
/// key that contains or is contained in its key (`llama4maverick17binstruct`
/// against the file's `llama4maverick17b`). Two such candidates is no match:
/// a wrong price is worse than none. Claude is skipped, priced elsewhere.
pub(super) fn attach_prices(
    learned: &mut HashMap<String, LearnedModel>,
    prices: &HashMap<String, ModelPricing>,
) {
    for (id, model) in learned.iter_mut() {
        if vendor_of(id) == Vendor::Anthropic {
            continue;
        }
        let keys = listing_keys(id, model);
        let exact = keys.iter().find_map(|k| prices.get(k));
        let found = exact.or_else(|| {
            let candidates: Vec<&ModelPricing> = keys
                .iter()
                .flat_map(|k| {
                    prices
                        .iter()
                        .filter(move |(name, _)| {
                            name.len() >= 4
                                && (k.contains(name.as_str()) || name.contains(k.as_str()))
                        })
                        .map(|(_, p)| p)
                })
                .collect();
            match candidates.as_slice() {
                [only] => Some(*only),
                _ => None,
            }
        });
        if let Some(pricing) = found {
            model.pricing = Some(*pricing);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// One product and its term, as the file spells them.
    fn product(
        sku: &str,
        model: &str,
        inference_type: &str,
        usagetype: &str,
        extra: &[(&str, &str)],
        unit: &str,
        usd: &str,
    ) -> (Value, Value) {
        let mut attributes = json!({
            "model": model,
            "inferenceType": inference_type,
            "usagetype": usagetype,
        });
        for (key, value) in extra {
            attributes[*key] = json!(value);
        }
        let term = json!({
            format!("{sku}.JRTCKXETXF"): {
                "priceDimensions": {
                    format!("{sku}.JRTCKXETXF.6YS6EN2CT7"): {
                        "unit": unit,
                        "pricePerUnit": { "USD": usd },
                    }
                }
            }
        });
        (json!({ "sku": sku, "attributes": attributes }), term)
    }

    fn file(products: Vec<(&str, Value, Value)>) -> Value {
        let mut p = serde_json::Map::new();
        let mut t = serde_json::Map::new();
        for (sku, product, term) in products {
            p.insert(sku.to_string(), product);
            t.insert(sku.to_string(), term);
        }
        json!({ "products": p, "terms": { "OnDemand": t } })
    }

    #[test]
    fn the_standard_on_demand_token_rates_are_read_per_model() {
        let rows = vec![
            (
                "A",
                product(
                    "A",
                    "Nova Pro",
                    "Input tokens",
                    "USE1-NovaPro-input-tokens",
                    &[("feature", "On-demand Inference")],
                    "1K tokens",
                    "0.0008000000",
                ),
            ),
            (
                "B",
                product(
                    "B",
                    "Nova Pro",
                    "Output tokens",
                    "USE1-NovaPro-output-tokens",
                    &[("feature", "On-demand Inference")],
                    "1K tokens",
                    "0.0032000000",
                ),
            ),
            (
                "C",
                product(
                    "C",
                    "Nova Pro",
                    "Prompt cache read input tokens",
                    "USE1-NovaPro-cache-read-input-token-count",
                    &[("feature", "On-demand Inference")],
                    "1K tokens",
                    "0.0002000000",
                ),
            ),
            (
                "D",
                product(
                    "D",
                    "Nova Pro",
                    "Prompt cache write input tokens",
                    "USE1-NovaPro-cache-write-input-token-count",
                    &[("feature", "On-demand Inference")],
                    "1K tokens",
                    "0.0000000000",
                ),
            ),
            // Batch, flex and latency-optimised rows are not the standard rate.
            (
                "E",
                product(
                    "E",
                    "Nova Pro",
                    "Input tokens",
                    "USE1-NovaPro-input-tokens-batch",
                    &[("feature", "Batch Inference")],
                    "1K tokens",
                    "0.0004000000",
                ),
            ),
            (
                "F",
                product(
                    "F",
                    "Nova Pro",
                    "Input tokens flex",
                    "USE1-NovaPro-input-tokens-flex",
                    &[("feature", "On-demand Inference")],
                    "1K tokens",
                    "0.0004000000",
                ),
            ),
            (
                "G",
                product(
                    "G",
                    "Nova Pro",
                    "Input tokens",
                    "USE1-NovaPro-input-tokens-latency-optimized",
                    &[("feature", "On-demand Inference")],
                    "1K tokens",
                    "0.0010000000",
                ),
            ),
            // The mantle listing of the same model, at the same price.
            (
                "H",
                product(
                    "H",
                    "gpt-oss-120b",
                    "Input tokens",
                    "USE1-openai.gpt-oss-120b-mantle-input-tokens-standard",
                    &[("service_tier", "standard")],
                    "1K tokens",
                    "0.0001500000",
                ),
            ),
            (
                "I",
                product(
                    "I",
                    "gpt-oss-120b",
                    "Output tokens",
                    "USE1-gpt-oss-120b-output-tokens",
                    &[("feature", "On-demand Inference")],
                    "1K tokens",
                    "0.0006000000",
                ),
            ),
            (
                "J",
                product(
                    "J",
                    "gpt-oss-120b",
                    "Input tokens",
                    "USE1-gpt-oss-120b-input-tokens",
                    &[("feature", "On-demand Inference")],
                    "1K tokens",
                    "0.0001500000",
                ),
            ),
            (
                "K",
                product(
                    "K",
                    "gpt-oss-120b",
                    "Input tokens",
                    "USE1-openai.gpt-oss-120b-mantle-input-tokens-priority",
                    &[("service_tier", "priority")],
                    "1K tokens",
                    "0.0003000000",
                ),
            ),
            // Half a rate card prices nothing.
            (
                "L",
                product(
                    "L",
                    "Lonely",
                    "Input tokens",
                    "USE1-Lonely-input-tokens",
                    &[("feature", "On-demand Inference")],
                    "1K tokens",
                    "0.001",
                ),
            ),
            (
                "Q",
                product(
                    "Q",
                    "Outonly",
                    "Output tokens",
                    "USE1-Outonly-output-tokens",
                    &[("feature", "On-demand Inference")],
                    "1K tokens",
                    "0.002",
                ),
            ),
            // A product priced by the image, not the token.
            (
                "M",
                product(
                    "M",
                    "Nova Canvas",
                    "Input tokens",
                    "USE1-NovaCanvas",
                    &[("feature", "On-demand Inference")],
                    "Images",
                    "0.04",
                ),
            ),
            // A product with no model name, and one with an unknown type.
            (
                "N",
                product(
                    "N",
                    "",
                    "Input tokens",
                    "USE1-x",
                    &[("feature", "On-demand Inference")],
                    "1K tokens",
                    "0.1",
                ),
            ),
            (
                "O",
                product(
                    "O",
                    "Nova Pro",
                    "Input Image Token Count",
                    "USE1-NovaPro-image",
                    &[("feature", "On-demand Inference")],
                    "1K tokens",
                    "0.1",
                ),
            ),
            // A rate that does not parse.
            (
                "P",
                product(
                    "P",
                    "Broken",
                    "Output tokens",
                    "USE1-Broken-output-tokens",
                    &[("feature", "On-demand Inference")],
                    "1K tokens",
                    "free",
                ),
            ),
        ];
        let mut f = file(rows.into_iter().map(|(sku, (p, t))| (sku, p, t)).collect());
        // The nameless product really has no `model` attribute.
        f["products"]["N"]["attributes"]
            .as_object_mut()
            .unwrap()
            .remove("model");
        let prices = parse_price_file(&f);
        let nova = prices["novapro"];
        assert_eq!(nova.input_per_mtok, 0.8);
        assert_eq!(nova.output_per_mtok, 3.2);
        assert_eq!(nova.cached_input_per_mtok, 0.2);
        // A zero write rate means "no extra charge", which is the input rate.
        assert_eq!(nova.cache_write_per_mtok, 0.8);
        let oss = prices["gptoss120b"];
        assert_eq!(oss.input_per_mtok, 0.15);
        assert_eq!(oss.output_per_mtok, 0.6);
        assert_eq!(oss.cached_input_per_mtok, 0.15);
        assert_eq!(prices.len(), 2);
    }

    #[test]
    fn a_file_without_products_or_terms_prices_nothing() {
        assert!(parse_price_file(&json!({})).is_empty());
        assert!(parse_price_file(&json!({ "products": {}, "terms": {} })).is_empty());
        // A product whose SKU has no term, or a term with no dimensions.
        let (p, _) = product(
            "Z",
            "M",
            "Input tokens",
            "u",
            &[("feature", "On-demand Inference")],
            "1K tokens",
            "1",
        );
        let f = json!({ "products": { "Z": p }, "terms": { "OnDemand": { "Z": { "t": {} } } } });
        assert!(parse_price_file(&f).is_empty());
        let f = json!({ "products": { "Z": p }, "terms": { "OnDemand": {} } });
        assert!(parse_price_file(&f).is_empty());
        assert_eq!(
            rate_per_mtok(Some(&json!({ "t": { "priceDimensions": {} } }))),
            None
        );
        assert_eq!(
            rate_per_mtok(Some(
                &json!({ "t": { "priceDimensions": { "d": { "unit": "1K tokens" } } } })
            )),
            None
        );
        assert_eq!(rate_per_mtok(Some(&json!(5))), None);
        assert_eq!(rate_per_mtok(Some(&json!({}))), None);
        assert_eq!(
            rate_per_mtok(Some(&json!({ "t": { "priceDimensions": 5 } }))),
            None
        );
        let numeric = json!({ "t": { "priceDimensions": { "d": { "unit": "1K tokens", "pricePerUnit": { "USD": 5 } } } } });
        assert_eq!(rate_per_mtok(Some(&numeric)), None);
        let euros = json!({ "t": { "priceDimensions": { "d": { "unit": "1K tokens", "pricePerUnit": { "EUR": "1" } } } } });
        assert_eq!(rate_per_mtok(Some(&euros)), None);
    }

    #[test]
    fn names_normalize_to_one_key() {
        assert_eq!(normalize("Llama 4 Maverick 17B"), "llama4maverick17b");
        assert_eq!(normalize("llama4-maverick-17b"), "llama4maverick17b");
        assert_eq!(normalize("DeepSeek v3.2"), "deepseekv32");
    }

    fn learned(name: Option<&str>) -> LearnedModel {
        LearnedModel {
            display_name: name.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn listing_keys_are_the_name_and_the_versionless_id_tail() {
        assert_eq!(
            listing_keys("us.amazon.nova-pro-v1:0", &learned(Some("Nova Pro"))),
            vec!["novapro"]
        );
        assert_eq!(
            listing_keys("openai.gpt-oss-120b-1:0", &learned(None)),
            vec!["gptoss120b"]
        );
        assert_eq!(
            listing_keys(
                "us.meta.llama4-maverick-17b-instruct-v1:0",
                &learned(Some("Llama 4 Maverick 17B Instruct"))
            ),
            vec!["llama4maverick17binstruct"]
        );
        assert_eq!(
            listing_keys("us.deepseek.r1-v1:0", &learned(Some("DeepSeek-R1"))),
            vec!["deepseekr1", "r1"]
        );
    }

    #[test]
    fn prices_attach_exactly_or_by_the_one_containing_name() {
        let mut prices = HashMap::new();
        prices.insert("novapro".to_string(), ModelPricing::flat(0.8, 3.2));
        prices.insert(
            "llama4maverick17b".to_string(),
            ModelPricing::flat(0.24, 0.97),
        );
        prices.insert("llama4scout17b".to_string(), ModelPricing::flat(0.17, 0.66));
        prices.insert("qwen332b".to_string(), ModelPricing::flat(0.15, 0.6));
        prices.insert("qwen3coder32b".to_string(), ModelPricing::flat(0.15, 0.6));
        prices.insert("gptoss120b".to_string(), ModelPricing::flat(0.15, 0.6));
        let mut models: HashMap<String, LearnedModel> = HashMap::new();
        models.insert(
            "us.amazon.nova-pro-v1:0".to_string(),
            learned(Some("Nova Pro")),
        );
        models.insert(
            "us.meta.llama4-maverick-17b-instruct-v1:0".to_string(),
            learned(Some("Llama 4 Maverick 17B Instruct")),
        );
        // Two containing candidates: no match.
        models.insert("qwen.qwen3-v1:0".to_string(), learned(Some("Qwen3")));
        // No candidate at all.
        models.insert(
            "mistral.mistral-large-3-v1:0".to_string(),
            learned(Some("Mistral Large 3")),
        );
        // Claude is priced elsewhere even when the file could match it.
        prices.insert("claudesonnet5".to_string(), ModelPricing::flat(1.0, 2.0));
        models.insert(
            "us.anthropic.claude-sonnet-5".to_string(),
            learned(Some("Claude Sonnet 5")),
        );
        attach_prices(&mut models, &prices);
        assert_eq!(
            models["us.amazon.nova-pro-v1:0"]
                .pricing
                .unwrap()
                .input_per_mtok,
            0.8
        );
        assert_eq!(
            models["us.meta.llama4-maverick-17b-instruct-v1:0"]
                .pricing
                .unwrap()
                .input_per_mtok,
            0.24
        );
        assert_eq!(models["qwen.qwen3-v1:0"].pricing, None);
        assert_eq!(models["mistral.mistral-large-3-v1:0"].pricing, None);
        assert_eq!(models["us.anthropic.claude-sonnet-5"].pricing, None);
    }

    #[test]
    fn the_price_file_is_per_region() {
        assert!(price_file_url("eu-west-1").ends_with("/current/eu-west-1/index.json"));
    }
}
