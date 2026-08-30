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

    /// Host to reach Anthropic on, when it is not Anthropic's own.
    ///
    /// For an enterprise gateway or a self-hosted proxy that speaks the same
    /// API on a different origin. `None` uses the public endpoint, which is
    /// what every existing config means.
    ///
    /// Per provider rather than one setting covering all of them, because a
    /// gateway usually fronts one family: pointing every provider at it would
    /// break the ones it does not serve.
    #[serde(default)]
    pub anthropic_base_url: Option<String>,

    /// Host to reach OpenAI on. See [`Self::anthropic_base_url`].
    #[serde(default)]
    pub openai_base_url: Option<String>,

    /// Host to reach Google AI on. See [`Self::anthropic_base_url`].
    #[serde(default)]
    pub google_base_url: Option<String>,

    /// Host to reach OpenRouter on. See [`Self::anthropic_base_url`].
    #[serde(default)]
    pub openrouter_base_url: Option<String>,

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

    /// Prompt-cache lifetime for Anthropic: `"5m"` (default) or `"1h"`.
    ///
    /// The longer one costs more to write and needs a beta header, which is
    /// sent for you. Worth it for a staged agent: stages routinely take longer
    /// than five minutes, so a prefix cached at the start of a run is cold by
    /// the time a later stage could have reused it.
    #[serde(default)]
    pub anthropic_cache_ttl: Option<leviath_providers::anthropic::CacheTtl>,

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
            .field("anthropic_cache_ttl", &self.anthropic_cache_ttl)
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

/// What backs a `[model_providers.<name>]` entry.
///
/// Absent from the file means [`Self::Script`], which is what every entry
/// written before the field existed is. Spelled out here rather than inferred
/// from which fields are set, so a config says what it means and a typo in
/// the value is a load error instead of a silently different provider.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelProviderKind {
    /// A Rhai provider script in `~/.leviath/providers/`.
    #[default]
    Script,
    /// A server speaking OpenAI's chat API, reached natively with no script:
    /// llama.cpp, vLLM, LM Studio, or a gateway.
    OpenaiCompatible,
}

impl ModelProviderKind {
    /// The spelling the config file uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::OpenaiCompatible => "openai-compatible",
        }
    }

    /// The kind a config file spelling names, if it is one.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "script" => Some(Self::Script),
            "openai-compatible" => Some(Self::OpenaiCompatible),
            _ => None,
        }
    }
}

/// A `[model_providers.<name>]` entry: a Rhai script provider's overrides, or
/// an OpenAI-compatible endpoint.
///
/// Every field is optional. For a script, keys not recognized below flow into
/// [`Self::extra`] and are forwarded to the script's `initialize(config)`
/// alongside `base_url` and `api_key`. For an endpoint, `base_url` is required
/// and `headers` and `models` are read; a key that would land in `extra` is
/// refused at load, since nothing would read it.
#[derive(Clone, Serialize, Deserialize, Default)]
pub struct ModelProviderConfig {
    /// What backs the entry. Absent means a script, so every existing config
    /// reads as it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ModelProviderKind>,

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

    /// Model ids this provider serves, so a blueprint entry naming one of them
    /// with no provider can resolve here.
    ///
    /// Only needed by a script with no `list_models`: one that has it is asked
    /// directly, and its answer is preferred over this list. Without either,
    /// the provider claims no models and can only be reached by a blueprint
    /// that pins it, which is what made a local model unreachable however the
    /// machine set `default_provider` (issue #598).
    /// `None` when the file does not mention it, `Some` when it does - including
    /// `Some(vec![])` for an explicit `serves = []`.
    ///
    /// The two are different states and were conflated as one empty `Vec`, which
    /// is what made the stale `serves = []` unremovable: a save-back writes
    /// whatever the field holds, and an empty `Vec` writes as `serves = []`
    /// however it got there. `None` writes nothing, which is what lets the
    /// `stale-empty-serves` migration take the line out.
    ///
    /// Skipping `None` does not break the invariant `Config::unknown_config_keys`
    /// rests on - "a field they set is a field that serializes". A field they set
    /// is `Some`, and `Some` always serializes, `serves = []` included. `None` is
    /// the field they did *not* set, which was never in the file to be reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serves: Option<Vec<String>>,

    /// Extra headers on every request to an OpenAI-compatible endpoint, as
    /// `Name = "value"`. A gateway that wants an organisation or routing header
    /// is the usual reason. Not read for a script.
    ///
    /// A `BTreeMap` so the file, the debug line and the API all list them in
    /// one order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, String>>,

    /// The model ids an OpenAI-compatible endpoint serves, for a server that
    /// does not answer `GET /models`. Read only when detection fails: a server
    /// that lists its models is believed over this. Not read for a script.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,

    /// Any additional keys, forwarded verbatim into the script's `initialize`.
    #[serde(flatten)]
    pub extra: HashMap<String, toml::Value>,
}

/// The keys an OpenAI-compatible endpoint entry reads, for the refusal above
/// to list. `script` is included because the struct accepts it on any entry,
/// though an endpoint never runs one.
const ENDPOINT_KEYS: &[&str] = &[
    "kind",
    "script",
    "api_key",
    "base_url",
    "rate_limit",
    "serves",
    "headers",
    "models",
];

impl ModelProviderConfig {
    /// The kind this entry is, with absent read as a script.
    pub fn kind(&self) -> ModelProviderKind {
        self.kind.unwrap_or_default()
    }

    /// Whether this entry is an OpenAI-compatible endpoint rather than a
    /// script.
    pub fn is_endpoint(&self) -> bool {
        self.kind() == ModelProviderKind::OpenaiCompatible
    }

    /// What is wrong with this entry, if anything, named against `name`.
    ///
    /// Checked at config load rather than at the first inference: an endpoint
    /// with nowhere to send a request is a config that cannot work, and the
    /// message should name the table to fix while the file is still in front
    /// of the person who wrote it.
    pub fn validate(&self, name: &str) -> anyhow::Result<()> {
        if self.is_endpoint()
            && self
                .base_url
                .as_deref()
                .is_none_or(|url| url.trim().is_empty())
        {
            anyhow::bail!(
                "[model_providers.{name}] has kind = \"openai-compatible\" but no \
                 base_url; set base_url to where the server listens, such as \
                 \"http://localhost:8080/v1\""
            );
        }
        // `extra` exists to reach a script's `initialize`. An endpoint has no
        // script, so a key landing there is one the endpoint will never read:
        // `modles` leaves it with no catalogue and `heaeders` sends nothing,
        // and both used to load clean. `Config::unknown_config_keys` cannot
        // catch them either, because `flatten` writes them straight back.
        if self.is_endpoint() && !self.extra.is_empty() {
            let mut keys: Vec<&str> = self.extra.keys().map(String::as_str).collect();
            keys.sort_unstable();
            anyhow::bail!(
                "[model_providers.{name}] has kind = \"openai-compatible\" and \
                 unknown key(s) {}; an endpoint reads only {}",
                keys.join(", "),
                ENDPOINT_KEYS.join(", ")
            );
        }
        Ok(())
    }

    /// The headers as an ordered list, for a provider constructor.
    pub fn header_pairs(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// Hand-written for the same reason [`ProviderConfig`]'s is, and with one extra
/// hazard: `extra` is forwarded verbatim into the script's `initialize`, which
/// is exactly where a second credential goes when a gateway wants one under its
/// own name, and an endpoint's `headers` carry the same thing. A derived `Debug`
/// would print `api_key` and every value in both, so this reports whether the
/// key is set and the *names* in `extra` and `headers` and nothing else - the
/// same shape `GatewayInfo` puts on the wire.
impl std::fmt::Debug for ModelProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Sorted: a `HashMap` iterates differently between two calls, and a
        // debug line that reorders itself is one nobody can diff.
        let mut extra_keys: Vec<&str> = self.extra.keys().map(String::as_str).collect();
        extra_keys.sort_unstable();
        // Header values are the same hazard as `extra`: an endpoint's second
        // credential is a header. Names only, for the same reason.
        let header_names: Vec<&str> = self
            .headers
            .iter()
            .flatten()
            .map(|(name, _)| name.as_str())
            .collect();
        f.debug_struct("ModelProviderConfig")
            .field("kind", &self.kind())
            .field("script", &self.script)
            .field("api_key", &redacted(&self.api_key))
            .field("base_url", &self.base_url)
            .field("rate_limit", &self.rate_limit)
            .field("header_names", &header_names)
            .field("models", &self.models)
            .field("extra_keys", &extra_keys)
            .finish()
    }
}
