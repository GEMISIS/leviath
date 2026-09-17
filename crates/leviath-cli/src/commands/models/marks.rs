//! What `lev models` marks on a row beyond its capabilities: prices that are
//! tiered or per unit, and a retention that conflicts with the config.

use leviath_providers::ModelPricing;
use leviath_providers::retention::{RetentionPolicy, RetentionSettings, Source};

/// The two price columns for a row.
///
/// A model billed by the unit (an image, a second of video) shows that price
/// in the input column and a dash for output. A model with a long-context
/// tier shows its base rates with a `+`: the tier is shown, never used to rank
/// or choose a model, and `lev models show` prints its rates.
pub(super) fn price_columns(pricing: Option<ModelPricing>) -> (String, String) {
    let Some(p) = pricing else {
        return (super::UNPRICED.to_owned(), super::UNPRICED.to_owned());
    };
    if let Some(unit) = p.unit
        && p.input_per_mtok == 0.0
        && p.output_per_mtok == 0.0
    {
        return (
            format!("{}/{}", super::fmt_rate(unit.usd), unit.unit.label()),
            "-".to_owned(),
        );
    }
    let plus = if p.long_context.is_some() { "+" } else { "" };
    (
        format!("{}{plus}", super::fmt_rate(p.input_per_mtok)),
        format!("{}{plus}", super::fmt_rate(p.output_per_mtok)),
    )
}

/// The footnote under a table where any row carries a `+`.
pub(super) const TIER_FOOTNOTE: &str = "+ a request whose prompt reaches the model's threshold is billed at a \
     higher rate for the whole request (`lev models show <model>`)";

/// The long-context line `lev models show` prints, when there is a tier.
pub(super) fn tier_line(pricing: &ModelPricing) -> Option<String> {
    let tier = pricing.long_context?;
    Some(format!(
        "${:.2} in / ${:.2} cached / ${:.2} out once a prompt reaches {}",
        tier.input_per_mtok,
        tier.cached_input_per_mtok,
        tier.output_per_mtok,
        super::fmt_tokens(tier.threshold_tokens)
    ))
}

/// The per-unit line `lev models show` prints, when there is a unit price.
pub(super) fn unit_line(pricing: &ModelPricing) -> Option<String> {
    let unit = pricing.unit?;
    let what = match unit.unit {
        leviath_providers::pricing::PriceUnit::Image => "image",
        leviath_providers::pricing::PriceUnit::VideoSecond => "second of video",
        leviath_providers::pricing::PriceUnit::AudioHour => "hour of audio",
        leviath_providers::pricing::PriceUnit::MillionChars => "million characters spoken",
        leviath_providers::pricing::PriceUnit::Clip => "clip of music",
    };
    Some(format!("${:.4} per {what}", unit.usd))
}

/// What `provider` keeps of `model` with the config's settings on top, and
/// the conflict with the config, when there is one.
///
/// A conflict is either of two things. Zero retention is asked for and the
/// model keeps something, so a stage on it is refused. Or a
/// `[model_capabilities]` entry declares the model keeps nothing and what the
/// provider read from its account says otherwise.
pub(super) fn retention_of(
    registry: &leviath_runtime::ProviderRegistry,
    settings: &RetentionSettings,
    provider: &str,
    model: &str,
) -> (RetentionPolicy, Option<String>) {
    let base = registry
        .get(provider)
        .and_then(|p| p.live_retention(model))
        .unwrap_or_else(|| leviath_providers::retention::builtin(provider, model));
    let resolved = leviath_providers::retention::resolve(base.clone(), provider, model, settings);
    let conflict = if resolved.source == Source::Override
        && resolved.is_zero()
        && base.source == Source::Live
        && !base.is_zero()
    {
        Some(format!(
            "[model_capabilities] declares zero retention, but {provider} says: {}",
            base.note
        ))
    } else if settings.zero_requested && !resolved.is_zero() {
        Some(format!("zero_retention is on, but {}", resolved.note))
    } else {
        None
    };
    (resolved, conflict)
}

/// Whether `entry` hands back something `pattern` covers.
pub(super) fn produces(mime: &leviath_providers::ModelMime, pattern: &str) -> bool {
    mime.output
        .iter()
        .any(|have| leviath_providers::capabilities::pattern_covers(have, pattern))
}

/// Say which providers could not be asked and why, under the table, where
/// the rows they explain were printed. Printed to stdout with the table: a
/// warning on stderr alone scrolled past, and the table above it looked like
/// a healthy provider's.
pub(super) fn print_failures(failures: &[(String, String)]) {
    if failures.is_empty() {
        return;
    }
    println!();
    println!(
        "Could not list models from these providers; their rows come from this build's table:"
    );
    for (name, why) in failures {
        println!("  x {name}: {why}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_providers::pricing::{PriceTier, PriceUnit, UnitPrice};
    use leviath_providers::retention::{Control, Retention};

    fn tiered() -> ModelPricing {
        ModelPricing {
            long_context: Some(PriceTier {
                threshold_tokens: 200_000,
                input_per_mtok: 2.5,
                cached_input_per_mtok: 0.4,
                cache_write_per_mtok: 2.5,
                output_per_mtok: 5.0,
            }),
            ..ModelPricing::flat(1.25, 2.5)
        }
    }

    #[test]
    fn prices_mark_a_tier_and_show_a_unit() {
        assert_eq!(price_columns(None), ("n/a".into(), "n/a".into()));
        assert_eq!(
            price_columns(Some(ModelPricing::flat(3.0, 15.0))),
            ("3.00".into(), "15.00".into())
        );
        assert_eq!(
            price_columns(Some(tiered())),
            ("1.25+".into(), "2.50+".into())
        );
        let image = ModelPricing::per_unit(UnitPrice {
            usd: 0.02,
            unit: PriceUnit::Image,
        });
        assert_eq!(
            price_columns(Some(image)),
            ("0.0200/img".into(), "-".into())
        );
        // A unit price beside token rates keeps the token columns.
        let both = ModelPricing {
            unit: image.unit,
            ..ModelPricing::flat(1.0, 2.0)
        };
        assert_eq!(price_columns(Some(both)).0, "1.00");
        assert!(TIER_FOOTNOTE.starts_with('+'));
    }

    #[test]
    fn show_lines_name_the_tier_and_every_unit() {
        assert_eq!(
            tier_line(&tiered()).unwrap(),
            "$2.50 in / $0.40 cached / $5.00 out once a prompt reaches 200K"
        );
        assert!(tier_line(&ModelPricing::flat(1.0, 1.0)).is_none());
        for (unit, word) in [
            (PriceUnit::Image, "image"),
            (PriceUnit::VideoSecond, "second of video"),
            (PriceUnit::AudioHour, "hour of audio"),
            (PriceUnit::MillionChars, "million characters spoken"),
            (PriceUnit::Clip, "clip of music"),
        ] {
            let line = unit_line(&ModelPricing::per_unit(UnitPrice { usd: 0.5, unit })).unwrap();
            assert!(line.ends_with(word), "{line}");
        }
        assert!(unit_line(&ModelPricing::flat(1.0, 1.0)).is_none());
    }

    #[test]
    fn a_model_that_keeps_data_conflicts_with_zero_retention() {
        let registry = leviath_runtime::ProviderRegistry::new();
        let mut settings = RetentionSettings::default();
        let (_, none) = retention_of(&registry, &settings, "meta", "muse-spark-1.3-contributor");
        assert!(none.is_none(), "no conflict without the switch");
        settings.zero_requested = true;
        let (policy, conflict) =
            retention_of(&registry, &settings, "meta", "muse-spark-1.3-contributor");
        assert_eq!(policy.retention, Retention::Indefinite);
        assert!(conflict.unwrap().contains("train"));
        let (_, local) = retention_of(&registry, &settings, "ollama", "llama3");
        assert!(local.is_none(), "local inference keeps nothing");
    }

    #[tokio::test]
    async fn a_zero_override_conflicts_only_with_a_live_answer() {
        struct Live;
        #[async_trait::async_trait]
        impl leviath_providers::Provider for Live {
            async fn infer(
                &self,
                _: &leviath_providers::InferenceRequest,
            ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
                Err(leviath_providers::ProviderError::ApiError(
                    "no inference here".into(),
                ))
            }
            async fn count_tokens(&self, _: &str, _: &str) -> usize {
                0
            }
            fn max_context_tokens(&self, _: &str) -> usize {
                0
            }
            fn name(&self) -> &str {
                "bedrock"
            }
            fn capabilities(&self, _: &str) -> leviath_providers::ModelCapabilities {
                Default::default()
            }
            fn live_retention(&self, _: &str) -> Option<RetentionPolicy> {
                Some(RetentionPolicy {
                    retention: Retention::Days(30),
                    control: Control::Account,
                    source: Source::Live,
                    note: "never served under mode none".into(),
                })
            }
        }
        let mut registry = leviath_runtime::ProviderRegistry::new();
        registry.register("bedrock".to_string(), std::sync::Arc::new(Live));
        let mut settings = RetentionSettings::default();
        settings
            .model_overrides
            .insert("openai.gpt-5.5".to_string(), Retention::Zero);
        let (_, conflict) = retention_of(&registry, &settings, "bedrock", "openai.gpt-5.5");
        assert!(conflict.unwrap().contains("never served under mode none"));
        // The same override against a documented answer is the operator's
        // word, not a conflict.
        let (_, trusted) = retention_of(&registry, &settings, "openai", "openai.gpt-5.5");
        assert!(trusted.is_none());
        // The stand-in's other answers, so it is not a half-read fixture.
        use leviath_providers::Provider as _;
        let request = leviath_providers::InferenceRequest {
            system: vec![],
            messages: vec![],
            model: "m".into(),
            max_tokens: 1,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        assert!(Live.infer(&request).await.is_err());
        assert_eq!(Live.count_tokens("x", "m").await, 0);
        assert_eq!(Live.max_context_tokens("m"), 0);
        assert_eq!(Live.name(), "bedrock");
        assert_eq!(Live.capabilities("m"), Default::default());
    }

    #[test]
    fn produces_reads_the_output_side() {
        let image = leviath_providers::ModelMime::new(&["text/*"], &["image/*"]);
        assert!(produces(&image, "image/png"));
        assert!(!produces(&image, "video/*"));
    }
}
