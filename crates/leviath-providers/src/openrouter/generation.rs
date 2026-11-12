//! What OpenRouter charged for a generation, read after the call is over.
//!
//! Every reply names its generation in a header, and OpenRouter keeps a
//! record under that name: the amount charged and the upstream's own token
//! counts, for a call that answered, one whose stream was cut and one that
//! was cancelled alike.

use super::OpenRouterProvider;
use crate::provider::{ProviderError, Result};

/// Note OpenRouter's id for the generation `response` is the answer to,
/// which arrives in a header with the status, before any token: what the
/// generation was charged can be looked up by it however the call ends.
pub(super) fn note(response: &reqwest::Response) {
    if let Some(id) = response
        .headers()
        .get("x-generation-id")
        .and_then(|value| value.to_str().ok())
    {
        crate::call_trace::note_call_id(id);
    }
}

/// What a generation record says its call used and was charged.
///
/// `total_cost` is the amount charged to the account, in US dollars. A call
/// made on the account's own upstream key is charged OpenRouter's fee there
/// and the upstream's price beside it, and the two together are what it
/// cost. The counts are the upstream's own (`native_tokens_*`), as a reply's
/// usage reports them.
fn usage_of(record: &serde_json::Value) -> Result<crate::TokenUsage> {
    use crate::usage::count;
    let data = record.get("data").unwrap_or(record);
    let charged = data
        .get("total_cost")
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| {
            ProviderError::InvalidResponse("the generation record names no total_cost".into())
        })?;
    let cached = count(data, "/native_tokens_cached");
    let mut usage = crate::TokenUsage::new(
        count(data, "/native_tokens_prompt").saturating_sub(cached),
        cached,
        0,
        count(data, "/native_tokens_completion"),
    )
    .whole();
    usage.terms.model = data
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let own_key = data.get("is_byok").and_then(serde_json::Value::as_bool) == Some(true);
    let upstream = data
        .get("upstream_inference_cost")
        .and_then(serde_json::Value::as_f64);
    Ok(match (own_key, upstream) {
        (false, _) => usage.with_reported_cost(Some(charged)),
        (true, Some(upstream)) => usage.with_reported_cost(Some(charged + upstream)),
        (true, None) => usage.with_unknown_cost(leviath_core::cost::CostUnknown::UnmodelledTerms),
    })
}

impl OpenRouterProvider {
    /// `GET /generation?id=`, with the same key the call was made with. The
    /// record is written some seconds after the call ends, and answers 404
    /// until then.
    pub(super) async fn generation_usage(
        &self,
        call_id: &str,
    ) -> Option<Result<crate::TokenUsage>> {
        // OpenRouter names a generation `gen-` and then letters, digits and
        // hyphens. Anything else is not a generation it keeps a record of (a
        // video job's id, say), and is never put in a URL.
        let generation = call_id.starts_with("gen-")
            && call_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !generation {
            return None;
        }
        let url = format!("{}/generation?id={call_id}", self.base_url);
        Some(
            crate::provider::fetch_listing(
                &self.client,
                &url,
                &self.listing_headers(),
                Some(crate::provider::SIDE_CALL_TIMEOUT_SECS),
            )
            .await
            .and_then(|record| usage_of(&record)),
        )
    }
}

#[cfg(test)]
#[path = "generation_tests.rs"]
mod tests;
