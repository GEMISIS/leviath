//! Exact token counts, from the two places Bedrock offers them.
//!
//! Bedrock's own `CountTokens` takes a Converse-shaped input and answers with
//! the count the model would be charged, for free, on the same runtime host
//! the inference goes to, keyed by the bare model id even when the inference
//! goes through a profile. It does not cover every model: the Claude models
//! that exist only behind a cross-region profile are not served by it, and
//! for those Anthropic's `count_tokens` route on the `bedrock-mantle` host
//! answers instead, also by the bare id. A model neither route counts falls
//! back to the local heuristic, exactly as Anthropic's and Gemini's counts
//! do when their endpoint is out of reach.

use super::BedrockProvider;
use super::catalog::{Vendor, bare_id, vendor_of, window_for};
use crate::failure::FailureKind;
use crate::provider::{ProviderError, Result};

/// The regions the `bedrock-mantle` host answers in. A configured region
/// outside this list counts on `us-east-1`: the count is free and carries
/// only the prompt text, and a model that needs this route is one whose
/// inference already crosses regions.
const MANTLE_REGIONS: &[&str] = &[
    "us-east-1",
    "us-east-2",
    "us-west-2",
    "ap-southeast-3",
    "ap-south-1",
    "ap-southeast-2",
    "ap-northeast-1",
    "eu-central-1",
    "eu-west-1",
    "eu-west-2",
    "eu-south-1",
    "eu-north-1",
    "sa-east-1",
    "us-gov-west-1",
];

/// The region whose mantle host carries every model the route serves.
/// Measured: Sonnet 5 counts on `us-east-1` and not on `us-west-2`, though
/// both have the host, so a count the region's host refuses goes here.
pub(super) const FALLBACK_MANTLE_REGION: &str = "us-east-1";

/// The host Anthropic's routes are served on for `region`.
pub(super) fn mantle_host(region: &str) -> String {
    let region = match MANTLE_REGIONS.contains(&region) {
        true => region,
        false => FALLBACK_MANTLE_REGION,
    };
    format!("https://bedrock-mantle.{region}.api.aws")
}

impl BedrockProvider {
    /// The exact count for `text` on `model`, from whichever route counts
    /// it, or the error that says neither does.
    ///
    /// The runtime route is tried first and remembered when it refuses a
    /// model, so a Claude that needs the Anthropic route pays one refused
    /// call per process rather than one per count.
    pub(super) async fn count_remote(&self, text: &str, model: &str) -> Result<usize> {
        if !self.count_route.contains(model) && card_counts(model) {
            match self.count_on_runtime(text, model).await {
                Ok(n) => return Ok(n),
                Err(e) if refuses_the_model(&e) => {
                    tracing::debug!(
                        model,
                        error = %e,
                        "Bedrock CountTokens does not count this model; remembering"
                    );
                    self.count_route.insert(model);
                }
                Err(e) => return Err(e),
            }
        }
        match vendor_of(model) {
            Vendor::Anthropic => {
                let hosts = self.mantle_hosts().ok_or_else(|| {
                    ProviderError::Other(
                        "a gateway is configured; Anthropic's count route is not reached through it"
                            .to_string(),
                    )
                })?;
                self.count_on_anthropic_route(&hosts, text, model).await
            }
            _ => Err(ProviderError::Other(format!(
                "Bedrock counts no tokens for {model}"
            ))),
        }
    }

    /// Where the runtime counts `model`.
    pub(super) fn count_url(&self, model: &str) -> String {
        format!(
            "{}/model/{}/count-tokens",
            self.runtime_base(),
            super::encode_model_id(bare_id(model))
        )
    }

    /// `POST /model/{id}/count-tokens` on the runtime host, with the bare
    /// model id: measured live, the route counts `anthropic.claude-sonnet-4-6`
    /// and refuses `us.anthropic.claude-sonnet-4-6`, the profile the same
    /// inference goes through.
    async fn count_on_runtime(&self, text: &str, model: &str) -> Result<usize> {
        let url = self.count_url(model);
        let body = serde_json::json!({
            "input": {
                "converse": {
                    "messages": [{ "role": "user", "content": [{ "text": text }] }]
                }
            }
        });
        let response = self
            .post_side_call(&url, &self.header_pairs(), &body)
            .await?;
        let value: serde_json::Value = crate::provider::decode_json(response).await?;
        value
            .get("inputTokens")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .ok_or_else(|| {
                ProviderError::InvalidResponse("CountTokens missing inputTokens".to_string())
            })
    }

    /// `POST /anthropic/v1/messages/count_tokens` on each of `hosts` in
    /// turn, with the bare model id Anthropic's routes know. A host that
    /// does not carry the model answers 404 and the next is asked; any other
    /// answer is final. `hosts` is never empty.
    pub(super) async fn count_on_anthropic_route(
        &self,
        hosts: &[String],
        text: &str,
        model: &str,
    ) -> Result<usize> {
        let mut result = self.count_on_mantle(&hosts[0], text, model).await;
        for host in &hosts[1..] {
            if !not_on_that_host(&result) {
                break;
            }
            tracing::debug!(model, host, "not on that mantle host; counting on the next");
            result = self.count_on_mantle(host, text, model).await;
        }
        result
    }

    /// One count on the mantle host at `base`.
    async fn count_on_mantle(&self, base: &str, text: &str, model: &str) -> Result<usize> {
        let url = format!("{base}/anthropic/v1/messages/count_tokens");
        let body = serde_json::json!({
            "model": bare_id(model),
            "messages": [{ "role": "user", "content": text }],
        });
        let headers = vec![
            ("x-api-key", self.api_key.clone()),
            ("anthropic-version", "2023-06-01".to_string()),
            ("content-type", "application/json".to_string()),
        ];
        let response = self.post_side_call(&url, &headers, &body).await?;
        let value: serde_json::Value = crate::provider::decode_json(response).await?;
        value
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .ok_or_else(|| {
                ProviderError::InvalidResponse("count_tokens missing input_tokens".to_string())
            })
    }
}

/// Whether AWS's card lists CountTokens for the model. A model the table
/// has not seen is tried, and a refusal remembered. Measured live: Nova
/// Micro and Sonnet 5, both listed as unsupported, answer "The provided
/// model doesn't support counting tokens".
fn card_counts(model: &str) -> bool {
    window_for(model).is_none_or(|row| row.count_tokens)
}

/// Whether a mantle host answered that it has no such model.
fn not_on_that_host(result: &Result<usize>) -> bool {
    matches!(result, Err(e) if e.failure_kind() == Some(FailureKind::NotFound))
}

/// Whether an error says the runtime route does not count this model, as
/// opposed to a failure that says nothing about the model: a 400 naming the
/// model, or a 404 for one the host has never heard of.
fn refuses_the_model(error: &ProviderError) -> bool {
    matches!(
        error.failure_kind(),
        Some(FailureKind::BadRequest | FailureKind::NotFound)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mantle_host_follows_the_region_where_it_exists() {
        assert_eq!(
            mantle_host("eu-west-1"),
            "https://bedrock-mantle.eu-west-1.api.aws"
        );
        assert_eq!(
            mantle_host("ca-central-1"),
            "https://bedrock-mantle.us-east-1.api.aws"
        );
    }

    #[test]
    fn only_a_missing_model_moves_a_count_to_the_next_host() {
        assert!(not_on_that_host(&Err(ProviderError::ApiError(
            "[not-found] HTTP 404: x".to_string()
        ))));
        assert!(!not_on_that_host(&Err(ProviderError::ApiError(
            "[bad-request] HTTP 400: x".to_string()
        ))));
        assert!(!not_on_that_host(&Ok(3)));
    }

    #[test]
    fn only_a_bad_request_or_not_found_marks_a_model_uncounted() {
        assert!(refuses_the_model(&ProviderError::ApiError(
            "[bad-request] HTTP 400: x".to_string()
        )));
        assert!(refuses_the_model(&ProviderError::ApiError(
            "[not-found] HTTP 404: x".to_string()
        )));
        assert!(!refuses_the_model(&ProviderError::ApiError(
            "[server-error] HTTP 500: x".to_string()
        )));
        assert!(!refuses_the_model(&ProviderError::RateLimitExceeded {
            retry_after_secs: None
        }));
    }
}
