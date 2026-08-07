//! `[providers]` and per-model overrides: which endpoint a stage's model resolves
//! to, and the credentials for it.
//!
//! `ProviderConfig` hand-writes its `Debug` so an API key cannot reach a log
//! through a derived one - the redaction is the reason this is not a derive.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Provider configuration.
///
/// `Debug` is hand-written (see below) so the keys cannot be printed.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Anthropic API key
    #[serde(default)]
    pub anthropic_api_key: Option<String>,

    /// OpenAI API key
    #[serde(default)]
    pub openai_api_key: Option<String>,

    /// Google AI (Gemini) API key
    #[serde(default)]
    pub google_api_key: Option<String>,

    /// Whether the Claude Code CLI transport is enabled.
    ///
    /// **Opt-in, and never selected for the user.** The CLI injects its own
    /// context into every call - including the account email address on the
    /// OAuth (subscription) path - which cannot be disabled. `lev setup` offers
    /// it and defaults to declining, so a user who presses Enter through the
    /// wizard ends up with it off.
    #[serde(default)]
    pub claude_code_enabled: bool,

    /// Path to the `claude` executable. `None` resolves `claude` on `PATH`.
    #[serde(default)]
    pub claude_code_binary: Option<String>,

    /// Reasoning effort for the Claude Code transport: `low` | `medium` |
    /// `high` | `xhigh` | `max`.
    ///
    /// Always sent explicitly. Left to itself the CLI picks `high` with adaptive
    /// thinking, spending output tokens and latency Leviath never asked for.
    /// `None` uses [`leviath_providers::claude_code::DEFAULT_EFFORT`].
    #[serde(default)]
    pub claude_code_effort: Option<String>,

    /// Host-wide failover chain, as `"provider/model"` entries, best first.
    ///
    /// Tried after a stage's own `models` list and the default model when the
    /// provider in use stops answering (out of credits, rejected key). Entries
    /// naming an unregistered provider are skipped, and a malformed entry is
    /// ignored with a warning rather than failing the load.
    ///
    /// `provider/model` rather than a bare provider name because a failover
    /// target needs a model to send; there is no sensible default per provider.
    /// A blueprint that names one model has nowhere to go without this, which
    /// is exactly how issue #201 took every agent down at once.
    #[serde(default)]
    pub fallback_order: Vec<String>,
}

/// Hand-written so the API keys can never be printed.
///
/// A `#[derive(Debug)]` here meant one `tracing::debug!(?config)` anywhere in
/// the workspace - or one `dbg!`, or an `anyhow` context that formats a struct
/// holding this - would put every provider key into the logs. Nothing did that
/// today, which is exactly when it is cheap to foreclose: the type now cannot
/// leak, so nobody has to remember not to.
///
/// Reports whether each key is *set*, which is what a debug line is actually
/// asking, and mirrors the `RedactedConfig` the `/api/config` handler returns.
impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("anthropic_api_key", &redacted(&self.anthropic_api_key))
            .field("openai_api_key", &redacted(&self.openai_api_key))
            .field("google_api_key", &redacted(&self.google_api_key))
            .field("claude_code_enabled", &self.claude_code_enabled)
            .field("claude_code_binary", &self.claude_code_binary)
            .field("claude_code_effort", &self.claude_code_effort)
            .field("fallback_order", &self.fallback_order)
            .finish()
    }
}

/// `"<set>"` or `"<unset>"` for an optional secret, for [`Debug`] output.
fn redacted(value: &Option<String>) -> &'static str {
    match value {
        Some(_) => "<set>",
        None => "<unset>",
    }
}

/// Optional overrides for a Rhai script provider, from `[model_providers.<name>]`.
///
/// Every field is optional. Keys not recognized below flow into [`Self::extra`]
/// and are forwarded to the script's `initialize(config)` alongside `base_url`
/// and `api_key`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelProviderConfig {
    /// Script filename stem or path. Defaults to `<name>.rhai` in the providers
    /// directory (`~/.leviath/providers/`).
    #[serde(default)]
    pub script: Option<String>,

    /// API key forwarded to the script as `config.api_key` (a script may instead
    /// read its own environment variable).
    #[serde(default)]
    pub api_key: Option<String>,

    /// Base URL forwarded to the script as `config.base_url`.
    #[serde(default)]
    pub base_url: Option<String>,

    /// Rate limit enforced by the Rust wrapper (requests/tokens per minute).
    #[serde(default)]
    pub rate_limit: Option<leviath_providers::RateLimitConfig>,

    /// Any additional keys, forwarded verbatim into the script's `initialize`.
    #[serde(flatten)]
    pub extra: HashMap<String, toml::Value>,
}
