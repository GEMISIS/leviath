//! One model a provider offers, as its listing or a compiled table describes it.

use crate::capabilities::ModelCapabilities;

/// Information about a model offered by a provider.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// Model identifier used in API requests
    pub id: String,

    /// Human-readable name for the model
    pub display_name: Option<String>,

    /// Name of the provider that owns this model
    pub provider: String,

    /// Capabilities of this model
    pub capabilities: ModelCapabilities,

    /// When the provider released it, as Unix seconds, if its listing says.
    pub released: Option<i64>,

    /// When the provider will withdraw it, as published, if its listing says.
    pub retires: Option<String>,

    /// What it costs, when the provider's listing quotes a rate.
    pub pricing: Option<crate::pricing::ModelPricing>,

    /// Whether this entry came from the provider's own listing rather than a
    /// table compiled into this build.
    pub learned: bool,

    /// What mime the model takes and can hand back. See [`super::Provider::mime`].
    pub mime: crate::capabilities::ModelMime,
}

impl ModelInfo {
    /// An entry from a compiled table: nothing learned, nothing dated, text
    /// in and text out until [`Self::with_mime`] says otherwise.
    pub fn new(
        id: impl Into<String>,
        provider: impl Into<String>,
        caps: ModelCapabilities,
    ) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            provider: provider.into(),
            capabilities: caps,
            released: None,
            retires: None,
            pricing: None,
            learned: false,
            mime: crate::capabilities::ModelMime::text_only(),
        }
    }

    /// The same entry, with what the model takes and produces filled in.
    pub fn with_mime(mut self, mime: crate::capabilities::ModelMime) -> Self {
        self.mime = mime;
        self
    }

    /// This entry with a display name.
    pub fn named(mut self, display_name: Option<String>) -> Self {
        self.display_name = display_name;
        self
    }

    /// This entry marked as read from the provider's own listing, for a
    /// listing that reports rows directly (a script's `list_models`) rather
    /// than through a [`crate::learned::LearnedModel`] record.
    ///
    /// Rows synthesized from configuration claims (`serves = [...]`,
    /// `[model_capabilities]`) are someone describing a model, not the
    /// provider answering for itself, and must not pass through here.
    pub fn listed(mut self) -> Self {
        self.learned = true;
        self
    }

    /// This entry marked as read from the listing, carrying what it said.
    pub fn learned_from(mut self, learned: &crate::learned::LearnedModel) -> Self {
        self.display_name = learned.display_name.clone();
        self.released = learned.released;
        self.retires = learned.retires.clone();
        self.pricing = learned.pricing;
        self.learned = true;
        self
    }
}
