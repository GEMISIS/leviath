//! What a provider's own listing said about its models.
//!
//! Every provider ships a compiled table of what its models can do, and every
//! table is out of date the day it ships: `gpt-5.5` refused a temperature the
//! table said it took, the OpenRouter section had no `claude-*-5` rows while
//! the gateway plainly served them, and a comparison set picked from the table
//! was a generation behind what the gateway had (#568). Each provider already
//! calls the one endpoint that knows better and kept a private map of a
//! different shape for what it chose to keep: sizes here, prices there, ids
//! somewhere else. This is the one shape they all fill now.
//!
//! The rule for a field is: `None` means "this provider's listing has no such
//! field", never "no". OpenAI's `/v1/models` says nothing about size, so an
//! OpenAI record has `None` for both limits and the table stays in charge of
//! them; OpenRouter's `supported_parameters` says outright whether a model
//! takes a temperature, so an OpenRouter record has `Some`. A provider that
//! cannot fill a field says so in a comment on its `prime_capabilities`, and
//! never guesses a `Some` to fill the gap.

use crate::capabilities::{LimitsSource, ModelCapabilities};
use crate::pricing::ModelPricing;
use crate::provider::ModelInfo;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One model as its provider's listing described it.
///
/// See the module doc for what `None` means. Not `Eq` because a rate is an
/// `f64`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LearnedModel {
    /// The name the provider shows people, when it publishes one.
    pub display_name: Option<String>,
    /// See [`ModelCapabilities::max_context_tokens`].
    pub max_context_tokens: Option<usize>,
    /// See [`ModelCapabilities::max_output_tokens`].
    pub max_output_tokens: Option<usize>,
    /// See [`ModelCapabilities::supports_temperature`].
    pub supports_temperature: Option<bool>,
    /// See [`ModelCapabilities::supports_tools`].
    pub supports_tools: Option<bool>,
    /// Whether the upstream bills a cache *write*, which is the signal that it
    /// expects an explicit `cache_control` marker. A model that quotes only a
    /// read price caches on its own by prefix, and marking it buys nothing or
    /// costs extra: measured on OpenRouter, `qwen3.6-plus` cached only when
    /// marked (7.2x cheaper on the second call) while marking `deepseek-v4-pro`
    /// made it dearer.
    pub explicit_cache_control: Option<bool>,
    /// What the provider charges, when the listing quotes it.
    pub pricing: Option<ModelPricing>,
    /// When the model was released, as Unix seconds.
    pub released: Option<i64>,
    /// When the provider will withdraw it, as the date string it published.
    pub retires: Option<String>,
}

impl LearnedModel {
    /// `base` with every field this record names replaced.
    ///
    /// `limits_source` moves to [`LimitsSource::Api`] only when a limit was
    /// learned: a record that says what parameters a model takes but nothing
    /// about its size would otherwise relabel a table guess as read from the
    /// API, and the label exists to say how much the number is worth.
    pub fn apply_to(&self, base: ModelCapabilities) -> ModelCapabilities {
        let names_a_limit = self.max_context_tokens.is_some() || self.max_output_tokens.is_some();
        ModelCapabilities {
            supports_temperature: self
                .supports_temperature
                .unwrap_or(base.supports_temperature),
            supports_tools: self.supports_tools.unwrap_or(base.supports_tools),
            max_context_tokens: self.max_context_tokens.unwrap_or(base.max_context_tokens),
            max_output_tokens: self.max_output_tokens.unwrap_or(base.max_output_tokens),
            limits_source: match names_a_limit {
                true => LimitsSource::Api,
                false => base.limits_source,
            },
            ..base
        }
    }
}

/// One provider's primed catalogue.
///
/// Empty until [`crate::Provider::prime_capabilities`] fills it, and empty for
/// good if the listing could not be read; both mean "the compiled table is in
/// charge", which is what happened before any provider learned anything.
/// Clones share the store, the way every provider's memo does, because the
/// provider is shared across every agent talking to it.
#[derive(Debug, Clone, Default)]
pub struct LearnedModels(Arc<Mutex<HashMap<String, LearnedModel>>>);

impl LearnedModels {
    /// Replace the whole catalogue with what a listing just said.
    pub fn replace(&self, models: HashMap<String, LearnedModel>) {
        *leviath_core::sync::lock(&self.0) = models;
    }

    /// What the listing said about `id`, if it mentioned it.
    pub fn get(&self, id: &str) -> Option<LearnedModel> {
        leviath_core::sync::lock(&self.0).get(id).cloned()
    }

    /// Whether the listing mentioned `id` at all.
    pub fn contains(&self, id: &str) -> bool {
        leviath_core::sync::lock(&self.0).contains_key(id)
    }

    /// Whether nothing has been learned, because priming has not run or
    /// could not reach the listing.
    pub fn is_empty(&self) -> bool {
        leviath_core::sync::lock(&self.0).is_empty()
    }

    /// Every id the listing named, in no particular order.
    pub fn ids(&self) -> Vec<String> {
        leviath_core::sync::lock(&self.0).keys().cloned().collect()
    }

    /// Every id the listing named, or `None` when it has not been read.
    ///
    /// The shape [`crate::Provider::served_catalog`] wants: `Some` is a
    /// complete list and `None` is "cannot say".
    pub fn catalog(&self) -> Option<Vec<String>> {
        let store = leviath_core::sync::lock(&self.0);
        (!store.is_empty()).then(|| store.keys().cloned().collect())
    }

    /// The listed id that `key` names, whole or by its last path segment.
    ///
    /// A gateway namespaces its ids (`openai/gpt-5.5`) and a blueprint names
    /// the model (`gpt-5.5`), so both spellings find the entry.
    pub fn find_by_key(&self, key: &str) -> Option<String> {
        leviath_core::sync::lock(&self.0)
            .keys()
            .find(|id| id.as_str() == key || id.rsplit('/').next() == Some(key))
            .cloned()
    }

    /// `base` corrected by what the listing said about `id`, if anything.
    pub fn corrected(&self, id: &str, base: ModelCapabilities) -> ModelCapabilities {
        match self.get(id) {
            Some(learned) => learned.apply_to(base),
            None => base,
        }
    }

    /// The catalogue as a listing, sorted by id so two calls agree.
    ///
    /// `capabilities` is the provider's own answer for an id, which already
    /// merges this store with its table and any operator override; asking it
    /// rather than reapplying the record here is what keeps a listing and an
    /// inference from describing the same model differently.
    pub fn to_model_infos(
        &self,
        provider: &str,
        capabilities: impl Fn(&str) -> ModelCapabilities,
    ) -> Vec<ModelInfo> {
        let mut entries: Vec<(String, LearnedModel)> = leviath_core::sync::lock(&self.0)
            .iter()
            .map(|(id, m)| (id.clone(), m.clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
            .into_iter()
            .map(|(id, learned)| {
                let caps = capabilities(&id);
                ModelInfo::new(id, provider, caps).learned_from(&learned)
            })
            .collect()
    }
}

/// Unix seconds for an RFC 3339 timestamp of the form `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Anthropic publishes `created_at` in exactly that form and nothing else in
/// this crate reads a date, so this is a reader for that form rather than a
/// dependency on a calendar crate. Anything else, including an offset other
/// than `Z`, is `None`: a release date is decoration on a listing, and a wrong
/// one is worse than a blank.
pub fn unix_seconds_from_rfc3339(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, time) = text.split_once('T')?;
    let time = time.strip_suffix('Z')?;
    let mut date_parts = date.split('-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (
        date_parts.next().flatten()?,
        date_parts.next().flatten()?,
        date_parts.next().flatten()?,
    );
    if date_parts.next().is_some() {
        return None;
    }
    let mut time_parts = time.split(':').map(|p| p.parse::<i64>().ok());
    let (hour, minute, second) = (
        time_parts.next().flatten()?,
        time_parts.next().flatten()?,
        time_parts.next().flatten()?,
    );
    if time_parts.next().is_some() {
        return None;
    }
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..61).contains(&second)
    {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
///
/// Howard Hinnant's `days_from_civil`, which is what every calendar library
/// reduces to.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// `YYYY-MM-DD` for a Unix timestamp, the way a listing prints a release date.
pub fn civil_date(unix_seconds: i64) -> String {
    let days = unix_seconds.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A named function rather than a closure at each call site: a closure
    /// the empty-store test hands over is a region that never runs.
    fn table_for(_: &str) -> ModelCapabilities {
        table()
    }

    fn table() -> ModelCapabilities {
        ModelCapabilities {
            supports_temperature: true,
            supports_tools: true,
            max_context_tokens: 8_192,
            max_output_tokens: 4_096,
            ..Default::default()
        }
    }

    #[test]
    fn a_record_that_says_nothing_changes_nothing() {
        assert_eq!(LearnedModel::default().apply_to(table()), table());
    }

    #[test]
    fn a_learned_limit_relabels_the_source_as_api() {
        let learned = LearnedModel {
            max_context_tokens: Some(1_000_000),
            ..Default::default()
        };
        let caps = learned.apply_to(table());
        assert_eq!(caps.max_context_tokens, 1_000_000);
        assert_eq!(caps.max_output_tokens, 4_096);
        assert_eq!(caps.limits_source, LimitsSource::Api);

        let output_only = LearnedModel {
            max_output_tokens: Some(64_000),
            ..Default::default()
        };
        let caps = output_only.apply_to(table());
        assert_eq!(caps.max_context_tokens, 8_192);
        assert_eq!(caps.max_output_tokens, 64_000);
        assert_eq!(caps.limits_source, LimitsSource::Api);
    }

    #[test]
    fn a_record_with_flags_but_no_limits_keeps_the_table_label() {
        let learned = LearnedModel {
            supports_temperature: Some(false),
            supports_tools: Some(false),
            ..Default::default()
        };
        let caps = learned.apply_to(table());
        assert!(!caps.supports_temperature);
        assert!(!caps.supports_tools);
        assert_eq!(caps.limits_source, LimitsSource::Builtin);
    }

    fn store() -> LearnedModels {
        let models = LearnedModels::default();
        models.replace(HashMap::from([
            (
                "openai/gpt-5.5".to_string(),
                LearnedModel {
                    max_context_tokens: Some(1_050_000),
                    ..Default::default()
                },
            ),
            ("claude-sonnet-5".to_string(), LearnedModel::default()),
        ]));
        models
    }

    #[test]
    fn an_empty_store_says_so_and_publishes_no_catalogue() {
        let models = LearnedModels::default();
        assert!(models.is_empty());
        assert!(models.ids().is_empty());
        assert_eq!(models.catalog(), None);
        assert_eq!(models.get("x"), None);
        assert!(!models.contains("x"));
        assert_eq!(models.corrected("x", table()), table());
        assert!(models.to_model_infos("p", table_for).is_empty());
    }

    #[test]
    fn a_primed_store_answers_from_what_it_learned() {
        let models = store();
        assert!(!models.is_empty());
        let mut ids = models.ids();
        ids.sort();
        assert_eq!(ids, ["claude-sonnet-5", "openai/gpt-5.5"]);
        let mut catalog = models.catalog().unwrap();
        catalog.sort();
        assert_eq!(catalog, ids);
        assert!(models.contains("claude-sonnet-5"));
        assert_eq!(
            models
                .corrected("openai/gpt-5.5", table())
                .max_context_tokens,
            1_050_000
        );
    }

    #[test]
    fn a_key_finds_its_id_whole_or_by_last_segment() {
        let models = store();
        assert_eq!(
            models.find_by_key("openai/gpt-5.5").as_deref(),
            Some("openai/gpt-5.5")
        );
        assert_eq!(
            models.find_by_key("gpt-5.5").as_deref(),
            Some("openai/gpt-5.5")
        );
        assert_eq!(
            models.find_by_key("claude-sonnet-5").as_deref(),
            Some("claude-sonnet-5")
        );
        assert_eq!(models.find_by_key("gpt-5"), None);
    }

    #[test]
    fn a_listing_is_sorted_and_carries_what_was_learned() {
        let models = LearnedModels::default();
        models.replace(HashMap::from([
            (
                "b".to_string(),
                LearnedModel {
                    display_name: Some("B".to_string()),
                    released: Some(86_400),
                    retires: Some("2027-01-01".to_string()),
                    pricing: Some(ModelPricing::flat(1.0, 2.0)),
                    ..Default::default()
                },
            ),
            ("a".to_string(), LearnedModel::default()),
        ]));
        let listed = models.to_model_infos("p", table_for);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, "a");
        assert!(listed[0].learned);
        assert_eq!(listed[0].display_name, None);
        assert_eq!(listed[1].id, "b");
        assert_eq!(listed[1].provider, "p");
        assert_eq!(listed[1].display_name.as_deref(), Some("B"));
        assert_eq!(listed[1].released, Some(86_400));
        assert_eq!(listed[1].retires.as_deref(), Some("2027-01-01"));
        assert_eq!(listed[1].pricing, Some(ModelPricing::flat(1.0, 2.0)));
    }

    #[test]
    fn rfc3339_reads_the_form_anthropic_publishes() {
        assert_eq!(unix_seconds_from_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            unix_seconds_from_rfc3339("1970-01-02T00:00:00Z"),
            Some(86_400)
        );
        assert_eq!(
            unix_seconds_from_rfc3339("2026-07-24T00:00:00Z"),
            Some(1_784_851_200)
        );
        assert_eq!(
            unix_seconds_from_rfc3339(" 2000-02-29T12:30:45Z "),
            Some(951_827_445)
        );
    }

    #[test]
    fn rfc3339_refuses_every_other_form() {
        for bad in [
            "",
            "2026-07-24",
            "2026-07-24T00:00:00",
            "2026-07-24T00:00:00+02:00",
            "2026-07-24T00:00:00.5Z",
            "2026-07T00:00:00Z",
            "2026-07-24-01T00:00:00Z",
            "2026-07-24T00:00Z",
            "2026-07-24T00:00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-07-32T00:00:00Z",
            "2026-07-24T24:00:00Z",
            "2026-07-24T00:60:00Z",
            "2026-07-24T00:00:61Z",
            "20x6-07-24T00:00:00Z",
            "2026-07-24Tzz:00:00Z",
            "2026-0x-24T00:00:00Z",
            "2026-07-2xT00:00:00Z",
            "2026-07-24T00:zz:00Z",
            "2026-07-24T00:00:zzZ",
            "2026T00:00:00Z",
            "2026-07-24T00Z",
            "2026-07-24T00:00Z",
        ] {
            assert_eq!(unix_seconds_from_rfc3339(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn civil_date_round_trips_through_the_day_count() {
        for (text, seconds) in [
            ("1970-01-01", 0),
            ("1969-12-31", -1),
            ("2000-02-29", 951_827_445),
            ("2026-07-24", 1_784_851_200),
            ("2026-08-28", 1_787_875_200),
            ("1999-12-31", 946_684_799),
        ] {
            assert_eq!(civil_date(seconds), text);
        }
        for text in ["1970-01-01", "2000-03-01", "2100-01-01", "2026-11-30"] {
            let seconds = unix_seconds_from_rfc3339(&format!("{text}T00:00:00Z")).unwrap();
            assert_eq!(civil_date(seconds), text);
        }
    }
}
