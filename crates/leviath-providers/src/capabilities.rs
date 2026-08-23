//! What a model can do, and the per-model corrections a user can apply.
//!
//! Separate from [`crate::provider`] for the same reason [`crate::pricing`] is:
//! that module is how a provider is CALLED, this one describes the model it
//! calls. They meet where a provider answers [`crate::Provider::capabilities`].

use serde::{Deserialize, Serialize};

/// Capabilities supported by a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    /// Whether the model supports temperature sampling
    pub supports_temperature: bool,

    /// Whether the model supports streaming responses
    pub supports_streaming: bool,

    /// Whether the model supports tool/function calling
    pub supports_tools: bool,

    /// Whether the model supports a system prompt
    pub supports_system_prompt: bool,

    /// Maximum number of context (input) tokens
    pub max_context_tokens: usize,

    /// Maximum number of output tokens
    pub max_output_tokens: usize,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 8192,
            max_output_tokens: 4096,
        }
    }
}

/// A `[model_capabilities]` entry: the fields an operator chose to change.
///
/// Every field is optional and unset means "leave it alone", so an entry names
/// only what it is correcting. The alternative - deserializing straight into
/// [`ModelCapabilities`] - has two failure modes, and this repo has now seen
/// both. Without field defaults a partial table fails to deserialize and the
/// override is dropped in silence (#338). With `#[serde(default)]` it succeeds
/// and quietly substitutes [`ModelCapabilities::default`] for everything the
/// operator did not mention, so correcting one boolean would drop a 400 000
/// token window to 8 192.
///
/// Merging onto the provider's own answer for that model is the only reading
/// that matches what the table looks like it does.
// No `Eq`: rates are `f64`, which has no total equality.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelCapabilityOverride {
    /// See [`ModelCapabilities::supports_temperature`].
    pub supports_temperature: Option<bool>,
    /// See [`ModelCapabilities::supports_streaming`].
    pub supports_streaming: Option<bool>,
    /// See [`ModelCapabilities::supports_tools`].
    pub supports_tools: Option<bool>,
    /// See [`ModelCapabilities::supports_system_prompt`].
    pub supports_system_prompt: Option<bool>,
    /// See [`ModelCapabilities::max_context_tokens`].
    pub max_context_tokens: Option<usize>,
    /// See [`ModelCapabilities::max_output_tokens`].
    pub max_output_tokens: Option<usize>,

    /// USD per million fresh input tokens. Overrides the shipped rate table,
    /// and is the only place a negotiated price can be recorded - no public
    /// pricing page shows one. See `crate::pricing::published_rates`.
    #[serde(default)]
    pub input_per_mtok: Option<f64>,
    /// USD per million cached input tokens. Defaults to the input rate.
    #[serde(default)]
    pub cached_input_per_mtok: Option<f64>,
    /// USD per million tokens written to cache. Defaults to the input rate.
    #[serde(default)]
    pub cache_write_per_mtok: Option<f64>,
    /// USD per million output tokens.
    #[serde(default)]
    pub output_per_mtok: Option<f64>,
}

impl ModelCapabilityOverride {
    /// `base` with every field this entry names replaced.
    pub fn apply_to(&self, base: ModelCapabilities) -> ModelCapabilities {
        ModelCapabilities {
            supports_temperature: self
                .supports_temperature
                .unwrap_or(base.supports_temperature),
            supports_streaming: self.supports_streaming.unwrap_or(base.supports_streaming),
            supports_tools: self.supports_tools.unwrap_or(base.supports_tools),
            supports_system_prompt: self
                .supports_system_prompt
                .unwrap_or(base.supports_system_prompt),
            max_context_tokens: self.max_context_tokens.unwrap_or(base.max_context_tokens),
            max_output_tokens: self.max_output_tokens.unwrap_or(base.max_output_tokens),
        }
    }
}

impl From<ModelCapabilities> for ModelCapabilityOverride {
    /// Every field named, for a caller that already has a complete set.
    fn from(c: ModelCapabilities) -> Self {
        Self {
            supports_temperature: Some(c.supports_temperature),
            supports_streaming: Some(c.supports_streaming),
            supports_tools: Some(c.supports_tools),
            supports_system_prompt: Some(c.supports_system_prompt),
            max_context_tokens: Some(c.max_context_tokens),
            max_output_tokens: Some(c.max_output_tokens),
            // Capabilities describe what a model can do, not what it charges,
            // so a set converted from them names no rates.
            input_per_mtok: None,
            cached_input_per_mtok: None,
            cache_write_per_mtok: None,
            output_per_mtok: None,
        }
    }
}
