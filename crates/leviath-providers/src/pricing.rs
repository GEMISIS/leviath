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
}
