//! How well a model call's cost is known.
//!
//! Every call a run sends is in exactly one of three classes. The provider
//! stated the amount it billed ([`CostClass::Reported`]). Or it gave the
//! call's whole usage and that was priced from rates that cover every term the
//! call was billed on ([`CostClass::Computed`]). Or neither could be had, and
//! the cost is [`CostClass::Unknown`] for a reason a [`CostUnknown`] names.
//!
//! Nothing here estimates. A call with no figure has none, and a total that
//! holds one such call is not a total.

use serde::{Deserialize, Serialize};

/// Which of the three classes a call's cost is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CostClass {
    /// The provider stated the amount it billed for the call.
    Reported,
    /// The provider gave the call's whole usage, priced from the rate table.
    Computed,
    /// The cost could not be learned.
    Unknown,
}

impl CostClass {
    /// The class as one lowercase word: `reported`, `computed`, `unknown`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Reported => "reported",
            Self::Computed => "computed",
            Self::Unknown => "unknown",
        }
    }
}

/// Why a call's cost is unknown.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CostUnknown {
    /// The reply said nothing of what the call used: no token counts, or for
    /// a media call no duration, size or count to price.
    NoUsage,
    /// No price is known for the model the call ran on.
    NoPrice,
    /// The call was billed on terms the rate table does not model: a service
    /// tier, a kind of token, a cache lifetime.
    UnmodelledTerms,
    /// The call was sent and then failed or was cut off, and the provider
    /// has no way to look up what it charged.
    NoLookup,
    /// The call was sent and then failed or was cut off, and the provider's
    /// record of it did not answer in the time allowed.
    LookupUnanswered,
    /// The call was recorded by an earlier release, which kept no cost for
    /// it.
    NotRecorded,
    /// The run has ended and the call is still out: what it cost is being
    /// settled, or was when the daemon stopped. Counted by whoever reads a
    /// run that has ended, for each call its list of calls out holds; a call
    /// that is settled is recorded under what it turned out to be. A call
    /// out on a run that is still going is in flight, and is not unknown.
    NotSettled,
}

impl CostUnknown {
    /// Every reason, in the order a listing shows them.
    pub const ALL: [Self; 7] = [
        Self::NoUsage,
        Self::NoPrice,
        Self::UnmodelledTerms,
        Self::NoLookup,
        Self::LookupUnanswered,
        Self::NotRecorded,
        Self::NotSettled,
    ];

    /// A short, stable name for wire formats: `no_usage`, `no_lookup`.
    pub fn label(self) -> &'static str {
        match self {
            Self::NoUsage => "no_usage",
            Self::NoPrice => "no_price",
            Self::UnmodelledTerms => "unmodelled_terms",
            Self::NoLookup => "no_lookup",
            Self::LookupUnanswered => "lookup_unanswered",
            Self::NotRecorded => "not_recorded",
            Self::NotSettled => "not_settled",
        }
    }

    /// The reason named by `label`, as [`Self::label`] spells it.
    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.label() == label)
    }

    /// The reason in plain words, as a clause that follows "unknown:".
    pub fn reason(self) -> &'static str {
        match self {
            Self::NoUsage => "the provider's reply did not say what the call used",
            Self::NoPrice => "no price is known for the model",
            Self::UnmodelledTerms => "the call was billed on terms the rate table does not cover",
            Self::NoLookup => {
                "the call failed or was cut off after it was sent, and the provider \
                 gives no way to look up what it charged"
            }
            Self::LookupUnanswered => {
                "the call failed or was cut off after it was sent, and the provider's \
                 record of it did not answer in time"
            }
            Self::NotRecorded => "an earlier release recorded the call without a cost",
            Self::NotSettled => {
                "the run ended with the call still out, and what it cost is not \
                 settled yet"
            }
        }
    }
}

/// What one call cost, with how that is known.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CallCost {
    /// The amount the provider said it billed, in US dollars.
    Reported(f64),
    /// The call's whole usage priced from the rate table, in US dollars.
    Computed(f64),
    /// No figure, and why.
    Unknown(CostUnknown),
}

impl CallCost {
    /// The amount, when there is one.
    pub fn usd(self) -> Option<f64> {
        match self {
            Self::Reported(usd) | Self::Computed(usd) => Some(usd),
            Self::Unknown(_) => None,
        }
    }

    /// Which class the cost is in.
    pub fn class(self) -> CostClass {
        match self {
            Self::Reported(_) => CostClass::Reported,
            Self::Computed(_) => CostClass::Computed,
            Self::Unknown(_) => CostClass::Unknown,
        }
    }

    /// Why there is no figure, when there is none.
    pub fn unknown(self) -> Option<CostUnknown> {
        match self {
            Self::Unknown(reason) => Some(reason),
            Self::Reported(_) | Self::Computed(_) => None,
        }
    }
}

/// How many calls have an unknown cost, by reason.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct UnknownCalls {
    /// Calls whose reply said nothing of what they used.
    #[serde(default)]
    pub no_usage: u32,
    /// Calls to a model with no known price.
    #[serde(default)]
    pub no_price: u32,
    /// Calls billed on terms the rate table does not cover.
    #[serde(default)]
    pub unmodelled_terms: u32,
    /// Calls that failed or were cut off, on a provider with no lookup.
    #[serde(default)]
    pub no_lookup: u32,
    /// Calls that failed or were cut off, whose lookup did not answer.
    #[serde(default)]
    pub lookup_unanswered: u32,
    /// Calls an earlier release recorded without a cost.
    #[serde(default)]
    pub not_recorded: u32,
    /// Calls still out on a run that has ended, whose cost is not settled
    /// yet.
    #[serde(default)]
    pub not_settled: u32,
}

impl UnknownCalls {
    /// The counter for `reason`.
    fn slot(&mut self, reason: CostUnknown) -> &mut u32 {
        match reason {
            CostUnknown::NoUsage => &mut self.no_usage,
            CostUnknown::NoPrice => &mut self.no_price,
            CostUnknown::UnmodelledTerms => &mut self.unmodelled_terms,
            CostUnknown::NoLookup => &mut self.no_lookup,
            CostUnknown::LookupUnanswered => &mut self.lookup_unanswered,
            CostUnknown::NotRecorded => &mut self.not_recorded,
            CostUnknown::NotSettled => &mut self.not_settled,
        }
    }

    /// How many calls are unknown for `reason`.
    pub fn count(&self, reason: CostUnknown) -> u32 {
        let mut copy = *self;
        *copy.slot(reason)
    }

    /// Count one more call unknown for `reason`.
    pub fn add(&mut self, reason: CostUnknown) {
        self.add_many(reason, 1);
    }

    /// Count `calls` more unknown for `reason`.
    pub fn add_many(&mut self, reason: CostUnknown, calls: u32) {
        let slot = self.slot(reason);
        *slot = slot.saturating_add(calls);
    }

    /// Fold `other`'s counts into these.
    pub fn merge(&mut self, other: &Self) {
        for (reason, calls) in other.reasons() {
            self.add_many(reason, calls);
        }
    }

    /// These counts less `before`'s: the calls counted since `before` was
    /// taken.
    pub fn since(&self, before: &Self) -> Self {
        let mut out = Self::default();
        for reason in CostUnknown::ALL {
            out.add_many(
                reason,
                self.count(reason).saturating_sub(before.count(reason)),
            );
        }
        out
    }

    /// These counts brought up to `calls` in all, for a record that counts
    /// its unknown calls without saying why for each: the ones with no
    /// reason are calls nothing priced. And a record whose cost is unknown
    /// (`cost_known` false) with no call counted at all is one an earlier
    /// release wrote without a cost.
    pub fn reconciled(mut self, calls: usize, cost_known: bool) -> Self {
        let calls = u32::try_from(calls).unwrap_or(u32::MAX);
        self.add_many(CostUnknown::NoPrice, calls.saturating_sub(self.total()));
        if !cost_known && self.is_empty() {
            self.add(CostUnknown::NotRecorded);
        }
        self
    }

    /// How many calls are unknown, for any reason.
    pub fn total(&self) -> u32 {
        self.reasons()
            .into_iter()
            .fold(0, |sum, (_, calls)| sum.saturating_add(calls))
    }

    /// Whether no call is unknown.
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    /// Each reason that has calls, with how many, in [`CostUnknown::ALL`]'s
    /// order.
    pub fn reasons(&self) -> Vec<(CostUnknown, u32)> {
        CostUnknown::ALL
            .into_iter()
            .map(|reason| (reason, self.count(reason)))
            .filter(|(_, calls)| *calls > 0)
            .collect()
    }

    /// The counts as one plain sentence part: `2 calls: no price is known for
    /// the model; 1 call: ...`. Empty when nothing is unknown.
    pub fn describe(&self) -> String {
        self.reasons()
            .into_iter()
            .map(|(reason, calls)| {
                let noun = match calls {
                    1 => "call",
                    _ => "calls",
                };
                format!("{calls} {noun}: {}", reason.reason())
            })
            .collect::<Vec<_>>()
            .join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_class_has_its_word() {
        let words: Vec<&str> = [CostClass::Reported, CostClass::Computed, CostClass::Unknown]
            .iter()
            .map(|c| c.label())
            .collect();
        assert_eq!(words, ["reported", "computed", "unknown"]);
    }

    #[test]
    fn every_reason_has_a_label_that_reads_back_and_a_sentence() {
        for reason in CostUnknown::ALL {
            assert_eq!(CostUnknown::from_label(reason.label()), Some(reason));
            assert!(!reason.reason().is_empty());
            // The label is what serde writes.
            assert_eq!(
                serde_json::to_string(&reason).unwrap(),
                format!("\"{}\"", reason.label())
            );
        }
        assert_eq!(CostUnknown::from_label("free"), None);
    }

    #[test]
    fn a_call_cost_says_its_amount_class_and_reason() {
        let reported = CallCost::Reported(0.5);
        assert_eq!(reported.usd(), Some(0.5));
        assert_eq!(reported.class(), CostClass::Reported);
        assert_eq!(reported.unknown(), None);

        let computed = CallCost::Computed(0.25);
        assert_eq!(computed.usd(), Some(0.25));
        assert_eq!(computed.class(), CostClass::Computed);
        assert_eq!(computed.unknown(), None);

        let unknown = CallCost::Unknown(CostUnknown::NoLookup);
        assert_eq!(unknown.usd(), None);
        assert_eq!(unknown.class(), CostClass::Unknown);
        assert_eq!(unknown.unknown(), Some(CostUnknown::NoLookup));
    }

    #[test]
    fn unknown_calls_count_by_reason() {
        let mut calls = UnknownCalls::default();
        assert!(calls.is_empty());
        assert_eq!(calls.describe(), "");
        for reason in CostUnknown::ALL {
            calls.add(reason);
        }
        calls.add(CostUnknown::NoPrice);
        assert_eq!(calls.total(), 8);
        assert_eq!(calls.count(CostUnknown::NoPrice), 2);
        assert_eq!(calls.count(CostUnknown::NoUsage), 1);
        assert_eq!(calls.reasons().len(), 7);

        let mut sum = UnknownCalls::default();
        sum.add(CostUnknown::NoLookup);
        sum.merge(&calls);
        assert_eq!(sum.count(CostUnknown::NoLookup), 2);
        assert_eq!(sum.total(), 9);
    }

    #[test]
    fn counts_since_an_earlier_reading_are_the_difference() {
        let mut before = UnknownCalls::default();
        before.add(CostUnknown::NoPrice);
        let mut now = before;
        now.add(CostUnknown::NoPrice);
        now.add(CostUnknown::NoLookup);
        let since = now.since(&before);
        assert_eq!(since.no_price, 1);
        assert_eq!(since.no_lookup, 1);
        assert_eq!(since.total(), 2);
        // Never below nothing.
        assert!(before.since(&now).is_empty());
    }

    #[test]
    fn a_record_with_no_reasons_is_reconciled_with_its_count() {
        // Three calls counted, one with a reason: the other two had no price.
        let mut known = UnknownCalls::default();
        known.add(CostUnknown::NoLookup);
        let all = known.reconciled(3, false);
        assert_eq!((all.no_lookup, all.no_price, all.total()), (1, 2, 3));
        // Nothing counted and no cost: recorded without one.
        let legacy = UnknownCalls::default().reconciled(0, false);
        assert_eq!(legacy.not_recorded, 1);
        // Nothing counted and a cost: nothing unknown.
        assert!(UnknownCalls::default().reconciled(0, true).is_empty());
    }

    #[test]
    fn the_description_counts_each_reason_in_words() {
        let mut calls = UnknownCalls::default();
        calls.add(CostUnknown::NoPrice);
        calls.add(CostUnknown::NoPrice);
        calls.add(CostUnknown::NoLookup);
        assert_eq!(
            calls.describe(),
            format!(
                "2 calls: {}; 1 call: {}",
                CostUnknown::NoPrice.reason(),
                CostUnknown::NoLookup.reason()
            )
        );
    }

    #[test]
    fn a_count_saturates_rather_than_wrapping() {
        let mut calls = UnknownCalls {
            no_usage: u32::MAX,
            ..Default::default()
        };
        calls.add(CostUnknown::NoUsage);
        calls.add(CostUnknown::NoPrice);
        assert_eq!(calls.no_usage, u32::MAX);
        assert_eq!(calls.total(), u32::MAX);
    }

    #[test]
    fn counts_missing_from_stored_json_read_as_zero() {
        let calls: UnknownCalls = serde_json::from_str(r#"{"no_price": 3}"#).unwrap();
        assert_eq!(calls.total(), 3);
    }
}
