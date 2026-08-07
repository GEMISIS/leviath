//! Which model a stage runs on, and what a user may override.
//!
//! Two levels: a [`ModelEntry`] names a provider and model, and [`ModelConfig`]
//! decides whether the user's own default may stand in for it.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A single model entry within a [`ModelConfig`] models list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelEntry {
    /// Provider name (e.g., "anthropic", "openai")
    pub provider: String,

    /// Model identifier (e.g., "claude-sonnet-4-6")
    pub model: String,
}

impl ModelEntry {
    /// One provider/model pair in a stage's fallback list.
    pub fn new(provider: String, model: String) -> Self {
        Self { provider, model }
    }
}

/// Model configuration for a stage.
///
/// Models are specified as an ordered priority list in `models`. The first
/// entry whose provider is registered at runtime is used. When
/// `allow_user_default` is true (the default), the user's configured default
/// model is tried as a last resort. When false, the stage fails if none of
/// the listed models are available.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Ordered list of models to try (first available wins).
    #[serde(default)]
    pub models: Vec<ModelEntry>,

    /// When true (default), fall back to the user's configured default model
    /// if none of the listed models are available.
    #[serde(default = "default_allow_user_default")]
    pub allow_user_default: bool,

    /// Optional parameters that apply to whichever model gets selected.
    #[serde(default)]
    pub parameters: HashMap<String, serde_json::Value>,

    /// Optional per-stage cap on the wall-clock time (in seconds) one inference
    /// for this stage may run - the whole call including retries. When set, it
    /// overrides the default job timeout; when `None`, the default applies.
    ///
    /// This lets a stage with slow first-token latency (e.g. a large-prompt
    /// analyze call) get a long cap while a quick iterative stage fails fast on
    /// a stalled connection instead of hanging for the full default.
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,
}

fn default_allow_user_default() -> bool {
    true
}

impl ModelConfig {
    /// Create a new model configuration with a single model entry.
    pub fn new(provider: String, model: String) -> Self {
        Self {
            models: vec![ModelEntry::new(provider, model)],
            allow_user_default: true,
            parameters: HashMap::new(),
            request_timeout_secs: None,
        }
    }

    /// Convenience: provider of the first model entry (for backward compat).
    pub fn provider(&self) -> &str {
        self.models
            .first()
            .map(|e| e.provider.as_str())
            .unwrap_or("anthropic")
    }

    /// Convenience: model name of the first model entry (for backward compat).
    pub fn model(&self) -> &str {
        self.models
            .first()
            .map(|e| e.model.as_str())
            .unwrap_or("claude-sonnet-4-6")
    }
}
