//! Which of OpenRouter's models have an endpoint that retains nothing.

use super::OpenRouterProvider;
use crate::provider::{ProviderError, Result};

impl OpenRouterProvider {
    /// GET `/endpoints/zdr`: every endpoint with a zero-retention policy,
    /// remembered as the set of model ids that have at least one. Answers
    /// how many models that is.
    pub async fn read_zdr_endpoints(&self) -> Result<usize> {
        let body = crate::provider::fetch_listing(
            &self.client,
            &format!("{}/endpoints/zdr", self.base_url),
            &self.listing_headers(),
            Some(crate::provider::SIDE_CALL_TIMEOUT_SECS),
        )
        .await?;
        let rows = body
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| ProviderError::InvalidResponse("Missing 'data' array".to_string()))?;
        let models: std::collections::HashSet<String> = rows
            .iter()
            .filter_map(|row| row.get("model_id")?.as_str().map(str::to_string))
            .collect();
        let count = models.len();
        *self
            .zdr_models
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(models);
        Ok(count)
    }

    /// Whether `model` has a zero-retention endpoint, once the list has
    /// been read: `None` before then. A variant suffix (`:free`, `:nitro`)
    /// names a routing preference on the same model, so it is dropped for
    /// the lookup when the full id is not listed.
    pub fn has_zdr_endpoint(&self, model: &str) -> Option<bool> {
        let listed = self
            .zdr_models
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let models = listed.as_ref()?;
        let plain = model.split_once(':').map_or(model, |(id, _)| id);
        Some(models.contains(model) || models.contains(plain))
    }
}
