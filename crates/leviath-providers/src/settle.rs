//! What a call that never answered cost.
//!
//! A call that fails, or that its caller cuts off, may still have been
//! billed. [`unanswered`] works out what is known about one from what the
//! call left behind (its [`Traced`] notes) and what its provider offers, in
//! this order:
//!
//! 1. A call whose request never left this machine cost nothing and is not a
//!    call at all.
//! 2. On rates that charge nothing (a local model, a subscription), nothing.
//! 3. A call that sent its request twice has no whole figure: whatever it
//!    learned is the second send's alone.
//! 4. The provider finished the call and said what it used before the
//!    failure: that usage, priced as any reply's is.
//! 5. The provider knows the call by an id and keeps a record of it: the
//!    record is asked for, for a bounded while, and priced.
//! 6. The provider refused the request outright, with an error status and
//!    nothing begun (no reply opened, no usage, no id): nothing, where that
//!    status says the request was never run or the provider's own terms say
//!    a failed request is not charged.
//! 7. Otherwise the cost is unknown, and says why.
//!
//! Nothing here estimates. A partial count from a stream that stopped early
//! is kept with the call and prices nothing.

use std::time::Duration;

use leviath_core::cost::{CallCost, CostUnknown};

use crate::call_trace::Traced;
use crate::pricing::{ModelPricing, TokenUsage};
use crate::provider::{Provider, ProviderError};

/// How long a provider's record of a call is asked for before the call's
/// cost is left unknown: the whole lookup, every request and every wait
/// between two, ends by then. OpenRouter answers 404 for a generation until
/// its record is written: about six seconds after the call ends most of the
/// time, and twelve to more than twenty-four on a slow day (measured
/// 2026-11-11 and 2026-11-12), so some records are not there by then.
pub const LOOKUP_PATIENCE: Duration = Duration::from_secs(15);

/// How long to wait between two requests for the same record.
pub const LOOKUP_INTERVAL: Duration = Duration::from_millis(1_500);

/// What a call that did not answer used and cost.
#[derive(Debug, Clone, PartialEq)]
pub struct Unanswered {
    /// What the call used, as far as anything said: the whole usage where
    /// the provider or its record gave it, the part that had arrived where
    /// the reply stopped early, and nothing where nothing arrived.
    pub usage: TokenUsage,
    /// What it cost, and how that is known.
    pub cost: CallCost,
    /// The provider's own id for the call, when it gave one.
    pub call_id: Option<String>,
}

/// How a call that did not answer ended.
#[derive(Debug, Clone, Copy)]
pub enum Ended<'a> {
    /// The provider, or the transport to it, failed the call.
    Failed(&'a ProviderError),
    /// The caller stopped waiting for it: a deadline, a cancel, a shutdown.
    /// Its request had been handed to the provider's client.
    CutOff,
}

/// What a call that answered with `usage` cost, priced by the reply's own
/// figure or else by the answering model's rates, with those rates.
///
/// A call that sent its request again after a send that went out (`seen`
/// says so) has no whole figure: the first send may have been billed, and
/// nothing the call reported covers it.
pub fn answered(
    provider: &dyn Provider,
    asked: &str,
    usage: &TokenUsage,
    seen: &Traced,
) -> (CallCost, Option<ModelPricing>) {
    let pricing = pricing_for(provider, asked, usage);
    let free = pricing.is_some_and(|rates| rates.is_free());
    let cost = match seen.resent && !free {
        true => CallCost::Unknown(CostUnknown::NoLookup),
        false => usage.cost(pricing.as_ref()),
    };
    (cost, pricing)
}

/// What `provider` charges for a call that asked for `asked` and was
/// answered by the model `usage` names: the answering model's price where
/// the provider has one, and the asked model's otherwise.
///
/// An alias answers under a dated name and a gateway may answer under
/// another's, and the reply's own word for what ran is the better key. A
/// name the provider has no price under (a local server's own spelling of a
/// model the operator priced by the name they call it) falls back to the
/// name that was asked for.
pub fn pricing_for(
    provider: &dyn Provider,
    asked: &str,
    usage: &TokenUsage,
) -> Option<ModelPricing> {
    usage
        .terms
        .model
        .as_deref()
        .filter(|answered| *answered != asked)
        .and_then(|answered| provider.pricing(answered))
        .or_else(|| provider.pricing(asked))
}

/// What the call `seen` describes cost, having ended as `ended` without an
/// answer, or `None` when its request never left this machine.
///
/// `patience` bounds the whole of any lookup, a request in flight included,
/// and `interval` spaces its requests. The lookup is the only waiting this
/// does.
pub async fn unanswered(
    provider: &dyn Provider,
    model: &str,
    ended: Ended<'_>,
    seen: &Traced,
    patience: Duration,
    interval: Duration,
) -> Option<Unanswered> {
    if let Ended::Failed(error) = ended
        && error.request_sent() == Some(false)
    {
        return None;
    }
    let partial = seen
        .usage
        .clone()
        .unwrap_or_else(|| TokenUsage::new(0, 0, 0, 0));
    let settled = |usage: TokenUsage, cost: CallCost| {
        Some(Unanswered {
            usage,
            cost,
            call_id: seen.call_id.clone(),
        })
    };
    if provider.pricing(model).is_some_and(|rates| rates.is_free()) {
        return settled(partial, CallCost::Computed(0.0));
    }
    // A request that went out twice may have been billed twice, and nothing
    // this call learned (its usage, its id, the status it ended on) says
    // what the first send came to.
    if seen.resent {
        return settled(partial, CallCost::Unknown(CostUnknown::NoLookup));
    }
    if seen.whole {
        let cost = partial.cost(pricing_for(provider, model, &partial).as_ref());
        return settled(partial, cost);
    }
    if let Some(call_id) = &seen.call_id {
        return match recorded(provider, call_id, model, patience, interval).await {
            Recorded::Usage(usage) => {
                let cost = usage.cost(pricing_for(provider, model, &usage).as_ref());
                settled(usage, cost)
            }
            Recorded::Unanswered => {
                settled(partial, CallCost::Unknown(CostUnknown::LookupUnanswered))
            }
            Recorded::NoRecord => settled(partial, CallCost::Unknown(CostUnknown::NoLookup)),
        };
    }
    // An error status is the whole of what the provider did with a request
    // only when nothing was begun: no reply opened, no usage arrived, and
    // (above) no id. A status named inside a reply that had started is not
    // a refusal.
    let nothing_began = !seen.began && seen.usage.is_none();
    let refused_free = match ended {
        Ended::Failed(error) => {
            nothing_began
                && (error.refused_unrun()
                    || (error.answered_status().is_some() && provider.bills_no_error_answer()))
        }
        Ended::CutOff => false,
    };
    match refused_free {
        true => settled(partial, CallCost::Computed(0.0)),
        false => settled(partial, CallCost::Unknown(CostUnknown::NoLookup)),
    }
}

/// What asking a provider for its record of a call came to.
enum Recorded {
    /// The record, as the call's whole usage.
    Usage(TokenUsage),
    /// The provider keeps records, and this one did not answer in time.
    Unanswered,
    /// The provider keeps no record the key in hand can read.
    NoRecord,
}

/// Ask `provider` for its record of `call_id` until it answers or
/// `patience` runs out, `interval` apart. The first request goes at once, so
/// a provider that keeps no record costs no wait at all, and a request still
/// in flight when `patience` runs out is dropped there.
async fn recorded(
    provider: &dyn Provider,
    call_id: &str,
    model: &str,
    patience: Duration,
    interval: Duration,
) -> Recorded {
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        let asked = tokio::time::timeout_at(deadline, provider.call_usage(call_id, model));
        let Ok(answer) = asked.await else {
            return Recorded::Unanswered;
        };
        match answer {
            None => return Recorded::NoRecord,
            Some(Ok(usage)) => return Recorded::Usage(usage),
            Some(Err(error)) => {
                let provider = provider.name();
                tracing::debug!(
                    provider,
                    %error,
                    "the provider's record of a call that did not answer is not readable yet"
                );
            }
        }
        if tokio::time::Instant::now() + interval > deadline {
            return Recorded::Unanswered;
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
#[path = "settle_tests.rs"]
mod tests;
