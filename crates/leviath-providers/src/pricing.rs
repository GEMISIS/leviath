//! What a model charges, and what a run has spent.
//!
//! Separate from [`crate::provider`] because they answer different questions:
//! that module is how a provider is called, this one is what the call costs.
//! They meet in exactly one place, [`TokenUsage::cost_usd`].
//!
//! The rule running through this module is that **an unknown cost must never
//! render as a number**. A partial total looks authoritative, gets quoted
//! onward, and understates by however much it silently skipped - worse than
//! admitting the figure is not available.

use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

/// Token usage breakdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Input tokens billed at the model's full input rate: **fresh input only**,
    /// with [`cached_tokens`](Self::cached_tokens) and
    /// [`cache_write_tokens`](Self::cache_write_tokens) excluded.
    ///
    /// The three input counts are DISJOINT and each is billed differently - a
    /// cache read costs a fraction of fresh input and a cache write costs more
    /// than it - so anything summing them has to add, never overlap.
    ///
    /// This has to be stated because the providers disagree and the field
    /// silently meant two things. Anthropic reports `input_tokens` already
    /// exclusive of both cache counts; the OpenAI shape reports a
    /// `prompt_tokens` that *includes* `prompt_tokens_details.cached_tokens`.
    /// Normalising here rather than at each reader is what keeps one arithmetic
    /// correct for both.
    pub prompt_tokens: usize,

    /// Tokens in the completion.
    pub completion_tokens: usize,

    /// Every token the call touched: fresh input + cache reads + cache writes +
    /// completion. Not `prompt_tokens + completion_tokens`, which omits both
    /// cache counts and so under-reports any cached call.
    pub total_tokens: usize,

    /// Input tokens served from the provider's cache, billed at the reduced
    /// cache-read rate (Anthropic: `cache_read_input_tokens`).
    #[serde(default)]
    pub cached_tokens: usize,

    /// Input tokens written into the provider's cache by this request, billed
    /// ABOVE the fresh input rate (Anthropic: `cache_creation_input_tokens`).
    #[serde(default)]
    pub cache_write_tokens: usize,

    /// What the provider said this call cost, in USD, when it says so at all.
    ///
    /// This is the only figure that is not an estimate. Rates times tokens is
    /// arithmetic on published numbers and drifts from the invoice for reasons
    /// outside this process - negotiated rates, promotional pricing, a gateway's
    /// margin, per-request minimums, a model silently rerouted to a different
    /// backend. When the provider reports its own number, that number is the
    /// answer and nothing here recomputes it.
    ///
    /// `None` where the API does not report cost, which is most of them; the
    /// caller then falls back to [`crate::Provider::pricing`].
    #[serde(default)]
    pub reported_cost_usd: Option<f64>,
}

impl TokenUsage {
    /// Build from the three disjoint input counts, deriving `total_tokens`.
    ///
    /// The one constructor for a parsed response, so a provider cannot ship a
    /// total that disagrees with its parts.
    ///
    /// Saturating: every count arrives from a provider's response body, and
    /// `overflow-checks` is on in release, so a gateway reporting a nonsense
    /// `prompt_tokens` would otherwise abort the daemon rather than one call.
    pub fn new(fresh: usize, cached: usize, cache_write: usize, completion: usize) -> Self {
        Self {
            prompt_tokens: fresh,
            completion_tokens: completion,
            total_tokens: fresh
                .saturating_add(cached)
                .saturating_add(cache_write)
                .saturating_add(completion),
            cached_tokens: cached,
            cache_write_tokens: cache_write,
            reported_cost_usd: None,
        }
    }

    /// The same, with the provider's own cost figure attached.
    pub fn with_reported_cost(mut self, cost_usd: Option<f64>) -> Self {
        self.reported_cost_usd = cost_usd;
        self
    }

    /// The derived total reconciled with a total the provider itself reported.
    ///
    /// The larger of the two stands. Parts in hand are evidence, so the
    /// derived figure never goes down; but a provider that reports only a
    /// total (a script forwarding an endpoint that says nothing finer) has
    /// all-zero parts, and deriving from those would zero the one figure it
    /// gave. For a provider whose total genuinely is the sum of its parts,
    /// this is a no-op.
    pub fn with_reported_total(mut self, total: usize) -> Self {
        self.total_tokens = self.total_tokens.max(total);
        self
    }

    /// All input tokens, however they were billed.
    pub fn input_tokens(&self) -> usize {
        self.prompt_tokens
            .saturating_add(self.cached_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    /// What this one call cost, or `None` when nothing can price it.
    ///
    /// The same rule [`CostTotals::add`] attributes by: the provider's own
    /// figure when it reported one, arithmetic from published rates when it did
    /// not, and nothing when neither is available. Kept here rather than written
    /// out again at each caller, so a per-call figure and a run total cannot
    /// disagree about the same call.
    pub fn priced_cost(&self, pricing: Option<&crate::pricing::ModelPricing>) -> Option<f64> {
        self.reported_cost_usd
            .or_else(|| pricing.map(|p| self.cost_usd(p)))
    }

    /// What this call cost, in USD, at `pricing`.
    ///
    /// Every input class is priced separately because providers charge them
    /// separately, and by very different multiples: a cache read is roughly a
    /// tenth of fresh input while a cache write is roughly a quarter more than
    /// it. Applying one rate to all input is wrong in both directions at once,
    /// and wrong by more the better a run caches - which is backwards, since a
    /// well-cached run is the one whose cost is most worth trusting.
    ///
    /// A model with a long-context tier bills the whole call at the tier's
    /// rates once its prompt (fresh, cached and written input together)
    /// reaches the tier's threshold, which is how xAI, Google and Anthropic
    /// each bill it.
    pub fn cost_usd(&self, pricing: &crate::pricing::ModelPricing) -> f64 {
        let per_mtok = |tokens: usize, rate: f64| tokens as f64 * rate / 1_000_000.0;
        let prompt = self.prompt_tokens + self.cached_tokens + self.cache_write_tokens;
        let (input, cached, written, output) = match pricing.long_context {
            Some(tier) if prompt >= tier.threshold_tokens => (
                tier.input_per_mtok,
                tier.cached_input_per_mtok,
                tier.cache_write_per_mtok,
                tier.output_per_mtok,
            ),
            _ => (
                pricing.input_per_mtok,
                pricing.cached_input_per_mtok,
                pricing.cache_write_per_mtok,
                pricing.output_per_mtok,
            ),
        };
        per_mtok(self.prompt_tokens, input)
            + per_mtok(self.cached_tokens, cached)
            + per_mtok(self.cache_write_tokens, written)
            + per_mtok(self.completion_tokens, output)
    }
}

/// What one model charges, in USD per million tokens.
///
/// Four rates rather than the usual two, because the cache classes are neither
/// free nor priced like fresh input, and folding them in is how a cost estimate
/// silently drifts from the invoice.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPricing {
    /// Fresh input, per million tokens.
    pub input_per_mtok: f64,
    /// Input served from cache. Typically a small fraction of the input rate.
    pub cached_input_per_mtok: f64,
    /// Input written into the cache. Typically ABOVE the input rate.
    pub cache_write_per_mtok: f64,
    /// Output, per million tokens.
    pub output_per_mtok: f64,
    /// The higher rates a call is billed at once its prompt reaches a size,
    /// when the model has such a tier. Shown beside the base rates, never
    /// used to rank or choose a model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_context: Option<PriceTier>,
    /// What the model charges per unit of media it makes or reads (an image,
    /// a second of video, an hour of audio), for a model billed that way
    /// rather than, or as well as, by the token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<UnitPrice>,
}

/// A long-context price tier: every rate a call is billed at once its prompt
/// reaches [`Self::threshold_tokens`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PriceTier {
    /// The prompt size, in tokens, at which the tier applies to the whole call.
    pub threshold_tokens: usize,
    /// Fresh input, per million tokens.
    pub input_per_mtok: f64,
    /// Input served from cache, per million tokens.
    pub cached_input_per_mtok: f64,
    /// Input written into the cache, per million tokens.
    pub cache_write_per_mtok: f64,
    /// Output, per million tokens.
    pub output_per_mtok: f64,
}

/// What a media model charges per unit.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UnitPrice {
    /// USD per one [`Self::unit`].
    pub usd: f64,
    /// What is counted.
    pub unit: PriceUnit,
}

/// The unit a [`UnitPrice`] counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceUnit {
    /// One generated or edited image.
    Image,
    /// One second of generated video.
    VideoSecond,
    /// One hour of audio read or written.
    AudioHour,
    /// One million characters of text spoken.
    MillionChars,
    /// One generated clip of music, whatever its length.
    Clip,
}

impl PriceUnit {
    /// The short label a price column shows after the amount: `0.02/img`.
    pub fn label(self) -> &'static str {
        match self {
            PriceUnit::Image => "img",
            PriceUnit::VideoSecond => "s",
            PriceUnit::AudioHour => "hr",
            PriceUnit::MillionChars => "M chr",
            PriceUnit::Clip => "clip",
        }
    }
}

impl UnitPrice {
    /// What `quantity` units cost.
    pub fn cost(self, quantity: f64) -> f64 {
        self.usd * quantity
    }
}

impl ModelPricing {
    /// Rates for a provider that bills input and output only, with no separate
    /// cache pricing: cache reads bill as input and writes cost nothing extra.
    pub fn flat(input_per_mtok: f64, output_per_mtok: f64) -> Self {
        Self {
            input_per_mtok,
            cached_input_per_mtok: input_per_mtok,
            cache_write_per_mtok: input_per_mtok,
            output_per_mtok,
            long_context: None,
            unit: None,
        }
    }

    /// Pricing for a model billed by the unit alone, with no token rates.
    pub fn per_unit(price: UnitPrice) -> Self {
        Self {
            unit: Some(price),
            ..Self::flat(0.0, 0.0)
        }
    }
}

/// The rows of `pricing/rates.toml`, parsed once.
///
/// A parse failure is a panic on first use rather than an error: the file is
/// compiled in, so a malformed one is a build of this crate that cannot price
/// anything, and the tests below catch it before it ships.
static RATE_TABLE: LazyLock<RateTable> = LazyLock::new(|| {
    toml::from_str(include_str!("../pricing/rates.toml"))
        .expect("pricing/rates.toml is well-formed; `cargo xtask prices` writes it")
});

/// The shape of `pricing/rates.toml`.
#[derive(Debug, Deserialize)]
struct RateTable {
    /// The day the rows were last refreshed, `YYYY-MM-DD`.
    read_on: String,
    /// Every token-priced row, in file order.
    #[serde(default)]
    rate: Vec<PublishedRate>,
    /// Every unit-priced row (media models), in file order.
    #[serde(default)]
    unit_rate: Vec<PublishedUnitRate>,
}

/// One unit-priced row of the shipped price table: a media model billed per
/// image, per second of video, per hour of audio, per million characters, or
/// per music clip.
///
/// LiteLLM publishes most of these, and `cargo xtask prices` rewrites those
/// rows; a row a person wrote (`manual`) is kept, and reported once it has
/// gone unchecked too long.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PublishedUnitRate {
    /// The provider the row applies to.
    pub provider: String,
    /// The model-id prefix the row covers; the longest matching prefix wins.
    pub prefix: String,
    /// What is counted.
    pub unit: PriceUnit,
    /// USD per unit.
    pub usd: f64,
    /// Where the figure came from: `litellm`, or `manual` for a row a person
    /// wrote.
    pub source: String,
    /// The day a person last checked the figure against the vendor's page.
    pub checked_on: String,
}

/// A long-context tier as the table writes it, beside its row.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct PublishedTier {
    /// The prompt size, in tokens, at which the tier applies.
    pub threshold: usize,
    /// Fresh input, USD per million tokens.
    pub input: f64,
    /// Input served from cache, USD per million tokens.
    pub cache_read: f64,
    /// Input written into the cache, USD per million tokens.
    pub cache_write: f64,
    /// Output, USD per million tokens.
    pub output: f64,
}

/// One row of the shipped price table: what a family of models charges, and
/// where the figure came from.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PublishedRate {
    /// The provider the row applies to: `anthropic`, `openai` or `google`.
    pub provider: String,
    /// The model-id prefix the row covers. A dated variant such as
    /// `gpt-5.5-2026-04-23` is covered by `gpt-5.5`; the longest matching
    /// prefix wins, so `gpt-5.4-mini` is not swallowed by `gpt-5.4`.
    pub prefix: String,
    /// Fresh input, USD per million tokens.
    pub input: f64,
    /// Input served from cache, USD per million tokens.
    pub cache_read: f64,
    /// Input written into the cache, USD per million tokens.
    pub cache_write: f64,
    /// Output, USD per million tokens.
    pub output: f64,
    /// Where the row came from: `both` when OpenRouter's catalogue and LiteLLM
    /// agreed, `openrouter` or `litellm` when only one listed it, `manual` for
    /// a row a person wrote, which `cargo xtask prices` never overwrites.
    pub source: String,
    /// The long-context tier, when the model has one.
    #[serde(default)]
    pub long_context: Option<PublishedTier>,
}

impl PublishedRate {
    /// The row as the four rates cost accounting bills by, with its tier.
    pub fn pricing(&self) -> ModelPricing {
        ModelPricing {
            input_per_mtok: self.input,
            cached_input_per_mtok: self.cache_read,
            cache_write_per_mtok: self.cache_write,
            output_per_mtok: self.output,
            long_context: self.long_context.map(|t| PriceTier {
                threshold_tokens: t.threshold,
                input_per_mtok: t.input,
                cached_input_per_mtok: t.cache_read,
                cache_write_per_mtok: t.cache_write,
                output_per_mtok: t.output,
            }),
            unit: None,
        }
    }
}

/// The unit-priced row that prices `model` at `provider`, longest prefix
/// first.
pub fn published_unit_rate(provider: &str, model: &str) -> Option<&'static PublishedUnitRate> {
    RATE_TABLE
        .unit_rate
        .iter()
        .filter(|row| row.provider == provider && model.starts_with(&row.prefix))
        .max_by_key(|row| row.prefix.len())
}

/// The day the rates in [`published_rates`] were last refreshed.
///
/// Shipped with the numbers so staleness is visible rather than assumed. A
/// build months old is quoting months-old prices, and the only honest thing to
/// do about that is say so.
pub fn rates_read_on() -> &'static str {
    &RATE_TABLE.read_on
}

/// The table row that prices `model` at `provider`, with its source, or `None`
/// when no row's prefix matches. The longest matching prefix wins.
pub fn published_rate(provider: &str, model: &str) -> Option<&'static PublishedRate> {
    RATE_TABLE
        .rate
        .iter()
        .filter(|row| row.provider == provider && model.starts_with(&row.prefix))
        .max_by_key(|row| row.prefix.len())
}

/// Published list prices for the providers whose APIs do not quote them.
///
/// ⚠️ **A snapshot, not a feed.** Anthropic, OpenAI and Google publish rates on
/// a web page with nothing programmatic behind it, so the rows live in
/// `pricing/rates.toml`, compiled into this crate and refreshed by `cargo xtask
/// prices`, which reads the vendors' list prices as OpenRouter's catalogue
/// carries them, cross-checks them against LiteLLM's table, and rewrites the
/// file when the two agree. A build cannot notice a repricing between
/// refreshes; [`rates_read_on`] says how old the figures are. They are a floor
/// under "no cost at all", not an authority:
///
/// * a per-model config entry overrides any row here, and is the right place
///   for a negotiated rate, which no public page will ever show;
/// * OpenRouter is absent on purpose - it reports each call's real cost and
///   serves live rates from its own catalogue, both of which beat a table;
/// * a cost computed from these is reported as computed, so
///   [`CostTotals::is_exact`] stays false and a reader can tell a reconstruction
///   from an invoice.
///
/// Cache columns are the vendors' own. Anthropic prices a 5-minute cache write
/// at 1.25x input and a read at 0.1x, which is the caching Leviath uses.
/// OpenAI and Google quote a cached-input rate and charge nothing extra to
/// write, so their write rate is the input rate.
///
/// A long-context tier travels with its row and is applied per call by
/// [`TokenUsage::cost_usd`], which sees the prompt size. Discounts that depend
/// on how a request was sent - batch, data residency, fast mode - are
/// deliberately not modelled: they need per-request state this table does not
/// see, and a wrong adjustment is worse than a plain list price.
///
/// A media model's unit price comes from the table's unit rows, alone or
/// beside its token rates.
pub fn published_rates(provider: &str, model: &str) -> Option<ModelPricing> {
    let unit = published_unit_rate(provider, model).map(|row| UnitPrice {
        usd: row.usd,
        unit: row.unit,
    });
    match (published_rate(provider, model), unit) {
        (Some(row), unit) => Some(ModelPricing {
            unit,
            ..row.pricing()
        }),
        (None, Some(unit)) => Some(ModelPricing::per_unit(unit)),
        (None, None) => None,
    }
}

impl crate::ModelCapabilityOverride {
    /// The rates this entry declares, or `None` when it declares no usable pair.
    ///
    /// Both sides are required. A total built from an input rate with no output
    /// rate is wrong in a way that still looks like a number, which is the one
    /// outcome the cost work exists to prevent - so half a rate card is treated
    /// as none of one.
    ///
    /// The cache rates default to the input rate, which is what a provider that
    /// does not price caching separately effectively charges.
    pub fn pricing(&self) -> Option<crate::ModelPricing> {
        let input = self.input_per_mtok?;
        let output = self.output_per_mtok?;
        Some(crate::ModelPricing {
            input_per_mtok: input,
            cached_input_per_mtok: self.cached_input_per_mtok.unwrap_or(input),
            cache_write_per_mtok: self.cache_write_per_mtok.unwrap_or(input),
            output_per_mtok: output,
            long_context: None,
            unit: None,
        })
    }
}

/// A run's money, kept so that "unknown" stays distinguishable from "zero".
///
/// A partial total is the dangerous shape: it looks authoritative, gets quoted
/// onward, and understates by however much it silently skipped. So the priced
/// part and the count of unpriced calls are carried separately, and
/// [`total_usd`](Self::total_usd) refuses to answer at all while any call went
/// unpriced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CostTotals {
    /// Summed cost of the calls that could be priced at all, by either route.
    pub priced_usd: f64,
    /// Calls the provider priced itself. Exact.
    pub reported_calls: usize,
    /// Calls priced from published rates. A reconstruction, not the invoice.
    pub computed_calls: usize,
    /// Calls with neither a reported cost nor known rates.
    pub unpriced_calls: usize,
}

impl CostTotals {
    /// Fold one call in.
    ///
    /// The provider's own figure wins over `pricing` whenever it gave one: that
    /// is the invoice, and rates times tokens is only a reconstruction of it.
    /// A call with neither is counted unpriced, never as zero.
    pub fn add(&mut self, usage: &TokenUsage, pricing: Option<&ModelPricing>) {
        match (usage.reported_cost_usd, pricing) {
            (Some(reported), _) => {
                self.priced_usd += reported;
                self.reported_calls += 1;
            }
            (None, Some(p)) => {
                self.priced_usd += usage.cost_usd(p);
                self.computed_calls += 1;
            }
            (None, None) => self.unpriced_calls += 1,
        }
    }

    /// Whether every priced call came from the provider rather than arithmetic.
    ///
    /// Worth surfacing next to the number: a total built entirely from reported
    /// costs is the invoice, while one built from rates is this process's best
    /// reconstruction of it, and a reader deserves to know which they have.
    pub fn is_exact(&self) -> bool {
        self.unpriced_calls == 0 && self.computed_calls == 0
    }

    /// The run's cost, or `None` when any call could not be priced.
    pub fn total_usd(&self) -> Option<f64> {
        (self.unpriced_calls == 0).then_some(self.priced_usd)
    }
}

#[cfg(test)]
mod tier_tests {
    use super::*;

    #[test]
    fn every_unit_has_its_short_label() {
        let labels: Vec<&str> = [
            PriceUnit::Image,
            PriceUnit::VideoSecond,
            PriceUnit::AudioHour,
            PriceUnit::MillionChars,
            PriceUnit::Clip,
        ]
        .iter()
        .map(|u| u.label())
        .collect();
        assert_eq!(labels, ["img", "s", "hr", "M chr", "clip"]);
    }

    fn tiered() -> ModelPricing {
        ModelPricing {
            long_context: Some(PriceTier {
                threshold_tokens: 200_000,
                input_per_mtok: 2.5,
                cached_input_per_mtok: 0.4,
                cache_write_per_mtok: 2.5,
                output_per_mtok: 5.0,
            }),
            cached_input_per_mtok: 0.2,
            ..ModelPricing::flat(1.25, 2.5)
        }
    }

    #[test]
    fn a_prompt_below_the_threshold_bills_the_base_rates() {
        let usage = TokenUsage::new(199_999, 0, 0, 1_000_000);
        let cost = usage.cost_usd(&tiered());
        assert!((cost - (0.199_999 * 1.25 + 2.5)).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn a_prompt_at_the_threshold_bills_the_whole_call_at_the_tier() {
        // Cached input counts toward the prompt: the threshold is the size of
        // what was sent, not what was fresh.
        let usage = TokenUsage::new(100_000, 100_000, 0, 1_000_000);
        let cost = usage.cost_usd(&tiered());
        assert!(
            (cost - (0.1 * 2.5 + 0.1 * 0.4 + 5.0)).abs() < 1e-9,
            "{cost}"
        );
    }

    #[test]
    fn a_unit_price_costs_its_quantity_and_labels_itself() {
        let price = UnitPrice {
            usd: 0.02,
            unit: PriceUnit::Image,
        };
        assert!((price.cost(3.0) - 0.06).abs() < 1e-12);
        let only = ModelPricing::per_unit(price);
        assert_eq!(only.input_per_mtok, 0.0);
        assert_eq!(only.unit, Some(price));
        let labels: Vec<_> = [
            PriceUnit::Image,
            PriceUnit::VideoSecond,
            PriceUnit::AudioHour,
            PriceUnit::MillionChars,
        ]
        .into_iter()
        .map(PriceUnit::label)
        .collect();
        assert_eq!(labels, ["img", "s", "hr", "M chr"]);
    }

    #[test]
    fn the_extra_fields_stay_out_of_json_when_unset_and_read_back_when_set() {
        let plain = serde_json::to_value(ModelPricing::flat(1.0, 2.0)).unwrap();
        assert!(plain.get("long_context").is_none(), "{plain}");
        assert!(plain.get("unit").is_none(), "{plain}");
        let rich = tiered();
        let back: ModelPricing =
            serde_json::from_value(serde_json::to_value(rich).unwrap()).unwrap();
        assert_eq!(back, rich);
        let old: ModelPricing = serde_json::from_str(
            r#"{"input_per_mtok":1,"cached_input_per_mtok":1,"cache_write_per_mtok":1,"output_per_mtok":2}"#,
        )
        .unwrap();
        assert_eq!(old, ModelPricing::flat(1.0, 2.0));
    }

    #[test]
    fn a_published_tier_and_unit_rows_price_like_their_views() {
        let row = PublishedRate {
            provider: "xai".into(),
            prefix: "grok-4.3".into(),
            input: 1.25,
            cache_read: 0.2,
            cache_write: 1.25,
            output: 2.5,
            source: "litellm".into(),
            long_context: Some(PublishedTier {
                threshold: 200_000,
                input: 2.5,
                cache_read: 0.4,
                cache_write: 2.5,
                output: 5.0,
            }),
        };
        assert_eq!(row.pricing(), tiered());
        for row in &RATE_TABLE.unit_rate {
            let priced = published_rates(&row.provider, &row.prefix).expect("a unit row prices");
            assert_eq!(priced.unit.map(|u| u.usd), Some(row.usd), "{row:?}");
            assert!(
                ["manual", "litellm"].contains(&row.source.as_str()),
                "{row:?}"
            );
            assert!(row.usd > 0.0, "{row:?}");
        }
        assert!(published_unit_rate("nobody", "nothing").is_none());
    }
}

#[cfg(test)]
mod cost_tests {
    use super::*;

    /// The rates as published, including Anthropic's asymmetric cache columns:
    /// a 5-minute write costs MORE than fresh input (1.25x) and a read costs a
    /// tenth of it. Flattening either way misprices every cached call, and a
    /// well-cached run is exactly the one whose cost is most worth trusting.
    #[test]
    fn anthropic_cache_rates_sit_either_side_of_the_input_rate() {
        let p = published_rates("anthropic", "claude-opus-5").expect("listed");
        assert_eq!(p.input_per_mtok, 5.0);
        assert_eq!(p.output_per_mtok, 25.0);
        assert!(
            p.cache_write_per_mtok > p.input_per_mtok,
            "writes cost more"
        );
        assert!(
            p.cached_input_per_mtok < p.input_per_mtok,
            "reads cost less"
        );
        assert_eq!(p.cache_write_per_mtok, 6.25);
        assert_eq!(p.cached_input_per_mtok, 0.5);
    }

    /// OpenAI and Google quote a cached-input rate and charge nothing extra to
    /// write, so their write rate is the input rate rather than a premium.
    #[test]
    fn providers_without_a_write_premium_charge_the_input_rate() {
        for (provider, model) in [("openai", "gpt-5.5"), ("google", "gemini-3.5-flash")] {
            let p = published_rates(provider, model).expect("listed");
            assert_eq!(
                p.cache_write_per_mtok, p.input_per_mtok,
                "{provider}/{model} has no write premium"
            );
            assert!(p.cached_input_per_mtok < p.input_per_mtok);
        }
    }

    /// Longest prefix wins. `gpt-5.4-mini` must not be swallowed by `gpt-5.4`,
    /// which would bill a mini model at three times its rate.
    #[test]
    fn a_longer_model_prefix_is_not_swallowed_by_a_shorter_one() {
        let mini = published_rates("openai", "gpt-5.4-mini").expect("listed");
        let full = published_rates("openai", "gpt-5.4").expect("listed");
        assert_eq!(mini.input_per_mtok, 0.75);
        assert_eq!(full.input_per_mtok, 2.5);
        assert!(mini.input_per_mtok < full.input_per_mtok);

        let lite = published_rates("google", "gemini-3.1-flash-lite").expect("listed");
        assert_eq!(lite.input_per_mtok, 0.25);
    }

    /// A model or provider the table does not name is unpriced, not free.
    #[test]
    fn an_unlisted_model_or_provider_is_unpriced() {
        assert_eq!(published_rates("anthropic", "claude-opus-9"), None);
        assert_eq!(published_rates("openai", "davinci"), None);
        // OpenRouter is absent on purpose: it reports each call's real cost and
        // serves live rates, both of which beat a transcription.
        assert_eq!(published_rates("openrouter", "x-ai/grok-4.6"), None);
        assert_eq!(published_rates("ollama", "qwen3.5:9b"), None);
    }

    /// Every row quotes both sides and a sane ordering, so a typo in the table
    /// cannot ship a model that bills output below input or a zero rate.
    #[test]
    fn every_row_is_internally_consistent() {
        let models = [
            ("anthropic", "claude-fable-5"),
            ("anthropic", "claude-opus-5"),
            ("anthropic", "claude-sonnet-5"),
            ("anthropic", "claude-haiku-4-5"),
            ("openai", "gpt-5.5"),
            ("openai", "gpt-5.4"),
            ("openai", "gpt-5.4-mini"),
            ("openai", "gpt-5.4-nano"),
            ("google", "gemini-3.5-flash"),
            ("google", "gemini-3.1-pro"),
            ("google", "gemini-3-flash"),
            ("google", "gemini-3.1-flash-lite"),
        ];
        // Two passes rather than a panicking closure: the closure's body is a
        // branch a passing test never enters, and the 100% gate counts it.
        let missing: Vec<&(&str, &str)> = models
            .iter()
            .filter(|(p, m)| published_rates(p, m).is_none())
            .collect();
        assert!(missing.is_empty(), "unlisted rows: {missing:?}");
        for p in models
            .iter()
            .filter_map(|(provider, model)| published_rates(provider, model))
        {
            let model = "row";
            assert!(p.input_per_mtok > 0.0, "{model} input");
            assert!(
                p.output_per_mtok > p.input_per_mtok,
                "{model} output > input"
            );
            assert!(p.cached_input_per_mtok > 0.0, "{model} cache read");
            assert!(
                p.cached_input_per_mtok <= p.input_per_mtok,
                "{model} cache read is not a premium"
            );
        }
    }

    /// The date ships with the numbers. Without it a reader cannot tell a
    /// current price from one transcribed a year ago.
    #[test]
    fn the_table_carries_the_day_it_was_read() {
        let read_on = rates_read_on();
        assert_eq!(read_on.len(), "YYYY-MM-DD".len());
        assert!(read_on.starts_with("202"));
    }

    /// A row names where its figures came from, and only from the four places
    /// `cargo xtask prices` knows, so a typo in the file cannot invent a fifth.
    #[test]
    fn every_row_names_a_known_source_and_provider() {
        let rows = &RATE_TABLE.rate;
        assert!(!rows.is_empty());
        let bad_source: Vec<&PublishedRate> = rows
            .iter()
            .filter(|r| !["openrouter", "litellm", "both", "manual"].contains(&r.source.as_str()))
            .collect();
        assert!(bad_source.is_empty(), "unknown source: {bad_source:?}");
        let bad_provider: Vec<&PublishedRate> = rows
            .iter()
            .filter(|r| {
                !["anthropic", "google", "meta", "openai", "xai"].contains(&r.provider.as_str())
            })
            .collect();
        assert!(
            bad_provider.is_empty(),
            "unknown provider: {bad_provider:?}"
        );
    }

    /// The file is sorted by provider then prefix with no duplicate prefix, so
    /// a hand edit and the refresh produce the same file for the same rows and
    /// a diff shows only what changed.
    #[test]
    fn the_file_is_sorted_and_has_no_duplicate_prefixes() {
        let keys: Vec<(&str, &str)> = RATE_TABLE
            .rate
            .iter()
            .map(|r| (r.provider.as_str(), r.prefix.as_str()))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(keys, sorted);
    }

    /// `lev models show` names a rate's source, which the row carries and the
    /// four-rate view does not.
    #[test]
    fn a_row_carries_its_source_and_prices_like_the_rate_view() {
        let row = published_rate("openai", "gpt-5.5-2026-04-23").expect("listed");
        assert_eq!(row.prefix, "gpt-5.5");
        assert!(!row.source.is_empty());
        assert_eq!(
            Some(row.pricing()),
            published_rates("openai", "gpt-5.5-2026-04-23")
        );
        assert_eq!(published_rate("openai", "davinci"), None);
    }

    /// Rates declared on a model's config entry are what the direct providers
    /// serve, since their APIs do not quote prices.
    #[test]
    fn a_config_entry_can_declare_rates() {
        let o = crate::ModelCapabilityOverride {
            input_per_mtok: Some(5.0),
            output_per_mtok: Some(25.0),
            cached_input_per_mtok: Some(0.5),
            cache_write_per_mtok: Some(6.25),
            ..Default::default()
        };
        let p = o.pricing().expect("both sides declared");
        assert_eq!(p.input_per_mtok, 5.0);
        assert_eq!(p.output_per_mtok, 25.0);
        assert_eq!(p.cached_input_per_mtok, 0.5);
        assert_eq!(p.cache_write_per_mtok, 6.25);
    }

    /// Cache rates left out default to the input rate.
    #[test]
    fn declared_rates_default_their_cache_sides_to_input() {
        let o = crate::ModelCapabilityOverride {
            input_per_mtok: Some(2.0),
            output_per_mtok: Some(8.0),
            ..Default::default()
        };
        let p = o.pricing().expect("both sides declared");
        assert_eq!(p.cached_input_per_mtok, 2.0);
        assert_eq!(p.cache_write_per_mtok, 2.0);
    }

    /// One side alone is treated as no rate card at all.
    #[test]
    fn half_a_rate_card_prices_nothing() {
        let only_in = crate::ModelCapabilityOverride {
            input_per_mtok: Some(5.0),
            ..Default::default()
        };
        let only_out = crate::ModelCapabilityOverride {
            output_per_mtok: Some(25.0),
            ..Default::default()
        };
        assert_eq!(only_in.pricing(), None);
        assert_eq!(only_out.pricing(), None);
        assert_eq!(crate::ModelCapabilityOverride::default().pricing(), None);
    }

    /// A set built from capabilities names no rates: what a model can do and
    /// what it charges are different questions.
    #[test]
    fn capabilities_converted_to_an_override_carry_no_rates() {
        let o = crate::ModelCapabilityOverride::from(crate::ModelCapabilities::default());
        assert_eq!(o.pricing(), None);
    }

    fn usage(fresh: usize, cached: usize, written: usize, out: usize) -> TokenUsage {
        TokenUsage::new(fresh, cached, written, out)
    }

    /// Each input class bills at its own rate. Applying the input rate to all
    /// of them is the mistake this type exists to prevent: a cache read is a
    /// fraction of fresh input and a cache write costs more than it, so one
    /// rate is wrong in both directions at once.
    #[test]
    fn every_input_class_bills_at_its_own_rate() {
        let p = ModelPricing {
            input_per_mtok: 10.0,
            cached_input_per_mtok: 1.0,
            cache_write_per_mtok: 12.5,
            output_per_mtok: 50.0,
            long_context: None,
            unit: None,
        };
        // 1M fresh, 1M cached, 1M written, 1M out.
        let u = usage(1_000_000, 1_000_000, 1_000_000, 1_000_000);
        assert_eq!(u.cost_usd(&p), 10.0 + 1.0 + 12.5 + 50.0);
        // Billing it all at the input rate would say 40.00, and a flat
        // input+output reading would say 30.00. Neither is the invoice.
        assert_ne!(u.cost_usd(&p), 40.0);
    }

    /// A provider that bills one input rate with no cache pricing.
    #[test]
    fn flat_pricing_bills_every_input_class_the_same() {
        let p = ModelPricing::flat(2.0, 8.0);
        let u = usage(500_000, 300_000, 200_000, 1_000_000);
        // All 1M input at 2.0, plus 1M output at 8.0.
        assert_eq!(u.cost_usd(&p), 2.0 + 8.0);
    }

    /// The provider's own figure beats anything computed, even when rates are
    /// available and disagree. Rates are a reconstruction; the reported number
    /// is what the account is actually charged.
    #[test]
    fn a_reported_cost_wins_over_computed_rates() {
        let p = ModelPricing::flat(1000.0, 1000.0);
        let u = usage(1_000_000, 0, 0, 1_000_000).with_reported_cost(Some(0.42));
        let mut totals = CostTotals::default();
        totals.add(&u, Some(&p));
        assert_eq!(
            totals.total_usd(),
            Some(0.42),
            "the invoice, not the estimate"
        );
        assert_eq!(totals.reported_calls, 1);
        assert_eq!(totals.computed_calls, 0);
        assert!(totals.is_exact());
    }

    /// With no reported cost, rates are used and the total is flagged as a
    /// reconstruction rather than the invoice.
    #[test]
    fn without_a_reported_cost_rates_are_used_but_marked_inexact() {
        let mut totals = CostTotals::default();
        totals.add(
            &usage(1_000_000, 0, 0, 0),
            Some(&ModelPricing::flat(3.0, 9.0)),
        );
        assert_eq!(totals.total_usd(), Some(3.0));
        assert_eq!(totals.computed_calls, 1);
        assert!(!totals.is_exact(), "computed from rates, not reported");
    }

    /// One unpriced call makes the whole total unknown. This is the property
    /// that matters: a partial total looks authoritative, gets quoted onward,
    /// and understates by however much it skipped.
    #[test]
    fn one_unpriced_call_makes_the_whole_total_unknown() {
        let mut totals = CostTotals::default();
        totals.add(
            &usage(1_000_000, 0, 0, 0),
            Some(&ModelPricing::flat(3.0, 9.0)),
        );
        assert_eq!(totals.total_usd(), Some(3.0));

        totals.add(&usage(500_000, 0, 0, 0), None);
        assert_eq!(
            totals.total_usd(),
            None,
            "not Some(3.0) with a call missing"
        );
        assert_eq!(totals.unpriced_calls, 1);
        // The priced part is still there for anyone who wants a lower bound,
        // it just is not offered as "the cost".
        assert_eq!(totals.priced_usd, 3.0);
    }

    /// A run that made no calls has spent nothing, and that zero is known -
    /// which is a different state from a run whose calls could not be priced.
    #[test]
    fn no_calls_is_a_known_zero_not_an_unknown() {
        assert_eq!(CostTotals::default().total_usd(), Some(0.0));
        assert!(CostTotals::default().is_exact());
    }

    /// Mixed providers accumulate: an exact call and a computed one sum, and
    /// the total is honest about not being wholly exact.
    #[test]
    fn reported_and_computed_calls_sum_but_the_total_is_not_exact() {
        let mut totals = CostTotals::default();
        totals.add(&usage(0, 0, 0, 0).with_reported_cost(Some(1.25)), None);
        totals.add(
            &usage(1_000_000, 0, 0, 0),
            Some(&ModelPricing::flat(2.0, 0.0)),
        );
        assert_eq!(totals.total_usd(), Some(3.25));
        assert_eq!(totals.reported_calls, 1);
        assert_eq!(totals.computed_calls, 1);
        assert!(!totals.is_exact());
    }
    /// The per-call figure and the running total agree, because a dashboard and
    /// a run record disagreeing about one call is the failure this exists to
    /// prevent.
    #[test]
    fn priced_cost_matches_what_the_totals_attribute() {
        let rates = ModelPricing {
            input_per_mtok: 3.0,
            cached_input_per_mtok: 0.3,
            cache_write_per_mtok: 3.75,
            output_per_mtok: 15.0,
            long_context: None,
            unit: None,
        };
        let computed = TokenUsage::new(1_000, 0, 0, 500);
        let mut totals = CostTotals::default();
        totals.add(&computed, Some(&rates));
        assert_eq!(computed.priced_cost(Some(&rates)), Some(totals.priced_usd));

        // A provider that priced the call itself wins over the rate card, on
        // both paths.
        let reported = TokenUsage::new(1_000, 0, 0, 500).with_reported_cost(Some(0.42));
        let mut totals = CostTotals::default();
        totals.add(&reported, Some(&rates));
        assert_eq!(reported.priced_cost(Some(&rates)), Some(totals.priced_usd));
        assert_eq!(reported.priced_cost(Some(&rates)), Some(0.42));

        // Nothing to price it with: no figure, and the totals count it unpriced.
        let mut totals = CostTotals::default();
        totals.add(&computed, None);
        assert_eq!(computed.priced_cost(None), None);
        assert_eq!(totals.unpriced_calls, 1);
    }

    /// Every count arrives from a provider response, so a gateway reporting a
    /// nonsense `prompt_tokens` must not abort the daemon in the constructor.
    #[test]
    fn token_usage_saturates_instead_of_aborting() {
        let usage = TokenUsage::new(usize::MAX, 1, 1, 1);
        assert_eq!(usage.total_tokens, usize::MAX);
        assert_eq!(usage.input_tokens(), usize::MAX);
    }
}
