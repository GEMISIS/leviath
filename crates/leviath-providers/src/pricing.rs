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
    pub fn new(fresh: usize, cached: usize, cache_write: usize, completion: usize) -> Self {
        Self {
            prompt_tokens: fresh,
            completion_tokens: completion,
            total_tokens: fresh + cached + cache_write + completion,
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

    /// All input tokens, however they were billed.
    pub fn input_tokens(&self) -> usize {
        self.prompt_tokens + self.cached_tokens + self.cache_write_tokens
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
    pub fn cost_usd(&self, pricing: &crate::pricing::ModelPricing) -> f64 {
        let per_mtok = |tokens: usize, rate: f64| tokens as f64 * rate / 1_000_000.0;
        per_mtok(self.prompt_tokens, pricing.input_per_mtok)
            + per_mtok(self.cached_tokens, pricing.cached_input_per_mtok)
            + per_mtok(self.cache_write_tokens, pricing.cache_write_per_mtok)
            + per_mtok(self.completion_tokens, pricing.output_per_mtok)
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
        }
    }
}

/// The day the rates in [`published_rates`] were read from the vendors' pages.
///
/// Shipped with the numbers so staleness is visible rather than assumed. A
/// build months old is quoting months-old prices, and the only honest thing to
/// do about that is say so.
pub const RATES_READ_ON: &str = "2026-08-23";

/// Published list prices for the providers whose APIs do not quote them.
///
/// ⚠️ **A snapshot, not a feed.** Anthropic, OpenAI and Google publish rates on
/// a web page with nothing programmatic behind it, so these were transcribed by
/// hand on [`RATES_READ_ON`] and cannot notice a repricing. They are a floor
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
/// Discounts and multipliers that depend on how a request was made - batch,
/// data residency, fast mode, long-context tiers - are deliberately not
/// modelled. They would need per-request state this table does not see, and a
/// wrong adjustment is worse than a plain list price.
pub fn published_rates(provider: &str, model: &str) -> Option<ModelPricing> {
    // (model prefix, input, cache read, cache write, output), longest prefix
    // first so `gpt-5.5` is not swallowed by `gpt-5`.
    let rows: &[(&str, f64, f64, f64, f64)] = match provider {
        "anthropic" => &[
            ("claude-fable-5", 10.0, 1.0, 12.5, 50.0),
            ("claude-opus-5", 5.0, 0.5, 6.25, 25.0),
            ("claude-opus-4-8", 5.0, 0.5, 6.25, 25.0),
            ("claude-opus-4-7", 5.0, 0.5, 6.25, 25.0),
            ("claude-opus-4-6", 5.0, 0.5, 6.25, 25.0),
            ("claude-sonnet-5", 2.0, 0.2, 2.5, 10.0),
            ("claude-sonnet-4-6", 3.0, 0.3, 3.75, 15.0),
            ("claude-haiku-4-5", 1.0, 0.1, 1.25, 5.0),
        ],
        "openai" => &[
            ("gpt-5.5", 5.0, 0.5, 5.0, 30.0),
            ("gpt-5.4-mini", 0.75, 0.075, 0.75, 4.5),
            ("gpt-5.4-nano", 0.2, 0.02, 0.2, 1.25),
            ("gpt-5.4", 2.5, 0.25, 2.5, 15.0),
        ],
        "google" => &[
            ("gemini-3.5-flash", 1.5, 0.15, 1.5, 9.0),
            ("gemini-3.1-pro", 2.0, 0.2, 2.0, 12.0),
            ("gemini-3.1-flash-lite", 0.25, 0.025, 0.25, 1.5),
            ("gemini-3-flash", 0.5, 0.05, 0.5, 3.0),
        ],
        _ => return None,
    };
    rows.iter()
        .find(|(prefix, ..)| model.starts_with(prefix))
        .map(|&(_, input, read, write, output)| ModelPricing {
            input_per_mtok: input,
            cached_input_per_mtok: read,
            cache_write_per_mtok: write,
            output_per_mtok: output,
        })
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
        assert_eq!(RATES_READ_ON.len(), "YYYY-MM-DD".len());
        assert!(RATES_READ_ON.starts_with("202"));
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
}
