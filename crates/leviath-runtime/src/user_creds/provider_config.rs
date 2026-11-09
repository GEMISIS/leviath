//! `[providers]`: the built-in providers' keys, hosts and headers, the
//! sign-in transports' switches, and the routing and retention preferences.
//!
//! `ProviderConfig` hand-writes its `Debug` so an API key cannot reach a log
//! through a derived one - the redaction is the reason this is not a derive.

use serde::{Deserialize, Serialize};

/// Provider configuration.
///
/// `Debug` is hand-written (see below) so the keys cannot be printed, and
/// `Default` is hand-written so it agrees with the serde defaults: a derived
/// one would give `codex_replay_reasoning: false` while a config read from
/// disk got `true`, and the two would disagree with nothing saying so.
#[derive(Clone, Serialize, Deserialize)]
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

    /// Meshy API key. Meshy is a generative 3D provider: reference images or
    /// an existing mesh in, a textured model out.
    #[serde(default)]
    pub meshy_api_key: Option<String>,

    /// AWS Bedrock API key, sent as a bearer token. Made in the Bedrock
    /// console under API keys; not an AWS access key. Env fallback
    /// `AWS_BEARER_TOKEN_BEDROCK`, the name AWS's own tooling reads.
    #[serde(default)]
    pub bedrock_api_key: Option<String>,

    /// xAI API key, for Grok models billed to an xAI API balance. Env
    /// fallback `XAI_API_KEY`.
    #[serde(default)]
    pub xai_api_key: Option<String>,

    /// Meta Model API key, for Muse models. Env fallback `META_AI_API_KEY`.
    /// Meta's own examples name the variable `MODEL_API_KEY`, which is too
    /// generic to read safely, so Leviath does not.
    #[serde(default)]
    pub meta_api_key: Option<String>,

    /// The AWS region whose Bedrock endpoints are called. Unset falls back
    /// to `AWS_REGION`, then `AWS_DEFAULT_REGION`, then `us-east-1`. Part of
    /// the address on every Bedrock host, so the one Bedrock setting a user
    /// has to get right.
    #[serde(default)]
    pub bedrock_region: Option<String>,

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

    /// Host to reach Meshy on. See [`Self::anthropic_base_url`].
    #[serde(default)]
    pub meshy_base_url: Option<String>,

    /// Host to reach the Bedrock runtime on, replacing
    /// `https://bedrock-runtime.<region>.amazonaws.com`. See
    /// [`Self::anthropic_base_url`]. With this set the live model listing,
    /// the price file and the token-count routes are not read: a gateway
    /// that fronts inference rarely fronts the rest.
    #[serde(default)]
    pub bedrock_base_url: Option<String>,

    /// Host to reach xAI on. See [`Self::anthropic_base_url`]. Also where a
    /// Grok subscription's requests go, since the two share the API.
    #[serde(default)]
    pub xai_base_url: Option<String>,

    /// Host to reach Meta's Model API on. See [`Self::anthropic_base_url`].
    #[serde(default)]
    pub meta_base_url: Option<String>,

    /// Extra headers on every request Anthropic's provider makes to its
    /// host: a gateway's own token, a tenant or cost-centre tag. Sent as
    /// written, after the provider's own headers. Meant for a gateway named
    /// in [`Self::anthropic_base_url`]; a value here is as often a credential
    /// as not, so `Debug` prints the names alone.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub anthropic_headers: std::collections::BTreeMap<String, String>,

    /// Extra headers for OpenAI's provider. See [`Self::anthropic_headers`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub openai_headers: std::collections::BTreeMap<String, String>,

    /// Extra headers for Google's provider. See [`Self::anthropic_headers`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub google_headers: std::collections::BTreeMap<String, String>,

    /// Extra headers for OpenRouter's provider. See [`Self::anthropic_headers`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub openrouter_headers: std::collections::BTreeMap<String, String>,

    /// Extra headers for Meshy's provider, on its API calls and not on the
    /// asset downloads, which go to signed URLs on another host. See
    /// [`Self::anthropic_headers`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub meshy_headers: std::collections::BTreeMap<String, String>,

    /// Extra headers for Bedrock's provider, on inference calls to the
    /// runtime origin (the one [`Self::bedrock_base_url`] replaces) and not
    /// on the AWS control-plane, price-file or count routes. See
    /// [`Self::anthropic_headers`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub bedrock_headers: std::collections::BTreeMap<String, String>,

    /// Extra headers for xAI's provider. See [`Self::anthropic_headers`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub xai_headers: std::collections::BTreeMap<String, String>,

    /// Extra headers for Meta's provider. See [`Self::anthropic_headers`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub meta_headers: std::collections::BTreeMap<String, String>,

    /// Whether to offer Ollama.
    ///
    /// Ollama needs no key and answers on a well-known local port, so it is
    /// opt-in: registered on every machine, it would make a bare model name in
    /// a blueprint resolvable against whatever happened to be running
    /// locally, which is a surprising place for a run to end up.
    ///
    /// Kept separate from `ollama_base_url` because the two say different
    /// things: this is "I chose Ollama", and the URL is "and it is not at the
    /// default address". Writing the default URL to mean the first would pin
    /// it, and `$OLLAMA_HOST` and the built-in default would both stop
    /// applying.
    ///
    /// A config that names a URL counts as having chosen it, so an install
    /// that set one before this field existed keeps working.
    #[serde(default)]
    pub ollama_enabled: bool,

    /// Whether to offer the Codex transport, which bills inference to a
    /// ChatGPT subscription rather than an API balance.
    ///
    /// Opt-in because the credential is a browser sign-in rather than a key,
    /// so enabling it
    /// without one configured would register a provider that cannot answer.
    /// `lev setup` offers it, and `lev auth login codex` does the sign-in.
    #[serde(default)]
    pub codex_enabled: bool,

    /// How Leviath identifies itself to the Codex route.
    ///
    /// The route has been observed to whitelist this header. `leviath` is
    /// accepted and is what ships; this exists so a user is not stuck waiting
    /// for a release if that changes. Nothing secret.
    #[serde(default)]
    pub codex_originator: Option<String>,

    /// Reasoning effort for the Codex transport: `none` | `minimal` | `low` |
    /// `medium` | `high` | `xhigh`. `None` uses `medium`.
    #[serde(default)]
    pub codex_reasoning_effort: Option<String>,

    /// Text verbosity for the Codex transport: `low` | `medium` | `high`.
    #[serde(default)]
    pub codex_verbosity: Option<String>,

    /// Whether to replay a turn's opaque reasoning token on the next request.
    ///
    /// On by default, and measured to work in every shape that matters. The
    /// switch exists because the route is undocumented: the day a replayed
    /// blob starts being refused, this turns it off without a release.
    #[serde(default = "default_true")]
    pub codex_replay_reasoning: bool,

    /// Whether to offer Grok billed to a subscription (SuperGrok, or X
    /// Premium+ on a linked X account) rather than an xAI API balance.
    ///
    /// Opt-in for the reason Codex is: the credential is a browser sign-in,
    /// and selecting it changes what gets billed. `lev setup` offers it, and
    /// `lev auth login grok` does the sign-in.
    #[serde(default)]
    pub grok_enabled: bool,

    /// Whether media parts (images, PDFs, audio, video) are uploaded to a
    /// provider's own file storage once and referenced by id afterwards, on
    /// the providers that have one.
    ///
    /// On by default: an upload is sent once rather than re-sent inline on
    /// every turn, and a provider's file limit is far larger than what fits
    /// inline. Off keeps every part inline, within each provider's inline
    /// limit. [`Self::zero_retention`] turns uploads off regardless, since an
    /// uploaded file is kept on the provider's servers until it is deleted.
    #[serde(default = "default_true")]
    pub file_uploads: bool,

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
    /// is how one provider running out of credits takes every agent down at
    /// once.
    #[serde(default)]
    pub fallback_order: Vec<String>,

    /// Ordered provider preference for a bare model name, best first, as plain
    /// provider names (e.g. `["codex", "openrouter", "openai"]`).
    ///
    /// When a blueprint names a model with no provider and more than one
    /// configured provider serves it, this decides which one wins. It
    /// generalizes [`default_provider`](super::UserSettings::default_provider) from a
    /// single front-runner into a full ordering; leave it empty and
    /// `default_provider` alone decides, exactly as before.
    ///
    /// Naming a provider here is also a deliberate choice to route bare names
    /// through it, so a subscription transport (Codex) that is
    /// otherwise reachable only by an explicit `provider/model` becomes eligible
    /// at the priority it is listed - which is how a user who prefers their
    /// subscription gets it used first. Bare provider names, not
    /// `provider/model`, because this is a preference over routes, not a
    /// failover target that needs a model to send (that is `fallback_order`).
    #[serde(default)]
    pub provider_order: Vec<String>,
    /// Ask every provider for zero data retention, and refuse a stage whose
    /// model cannot give it rather than send the request and hope. What each
    /// provider keeps, and how the request reaches it (a per-request field,
    /// an account setting, a contract), is `lev providers retention`.
    #[serde(default)]
    pub zero_retention: bool,
    /// Providers this organisation holds a zero data retention agreement
    /// with, by registry name (`openai`, `anthropic`, `google`). No API can
    /// read such a contract, so it is declared here; with it, the provider
    /// counts as keeping nothing. A model that retains regardless (Claude
    /// Fable 5, Mythos 5) is not moved by it.
    #[serde(default)]
    pub zero_retention_agreements: Vec<String>,
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
            .field("meshy_api_key", &redacted(&self.meshy_api_key))
            .field("bedrock_api_key", &redacted(&self.bedrock_api_key))
            .field("xai_api_key", &redacted(&self.xai_api_key))
            .field("meta_api_key", &redacted(&self.meta_api_key))
            // A region is not a secret.
            .field("bedrock_region", &self.bedrock_region)
            // Not redacted, deliberately: none of these are secrets, and the
            // grant they authenticate with lives outside this file entirely.
            .field("codex_enabled", &self.codex_enabled)
            .field("codex_originator", &self.codex_originator)
            .field("codex_reasoning_effort", &self.codex_reasoning_effort)
            .field("codex_verbosity", &self.codex_verbosity)
            .field("codex_replay_reasoning", &self.codex_replay_reasoning)
            .field("grok_enabled", &self.grok_enabled)
            .field("file_uploads", &self.file_uploads)
            .field("anthropic_cache_ttl", &self.anthropic_cache_ttl)
            .field("fallback_order", &self.fallback_order)
            .field("zero_retention", &self.zero_retention)
            .field("zero_retention_agreements", &self.zero_retention_agreements)
            // A header value is a credential as often as not, so the names
            // alone say what is configured.
            .field("anthropic_headers", &header_names(&self.anthropic_headers))
            .field("openai_headers", &header_names(&self.openai_headers))
            .field("google_headers", &header_names(&self.google_headers))
            .field(
                "openrouter_headers",
                &header_names(&self.openrouter_headers),
            )
            .field("meshy_headers", &header_names(&self.meshy_headers))
            .field("bedrock_headers", &header_names(&self.bedrock_headers))
            .field("xai_headers", &header_names(&self.xai_headers))
            .field("meta_headers", &header_names(&self.meta_headers))
            .finish()
    }
}

/// The default for a flag that is on unless someone turns it off.
fn default_true() -> bool {
    true
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            anthropic_api_key: None,
            openai_api_key: None,
            google_api_key: None,
            meshy_api_key: None,
            bedrock_api_key: None,
            xai_api_key: None,
            meta_api_key: None,
            bedrock_region: None,
            anthropic_base_url: None,
            openai_base_url: None,
            google_base_url: None,
            openrouter_base_url: None,
            meshy_base_url: None,
            bedrock_base_url: None,
            xai_base_url: None,
            meta_base_url: None,
            anthropic_headers: std::collections::BTreeMap::new(),
            openai_headers: std::collections::BTreeMap::new(),
            google_headers: std::collections::BTreeMap::new(),
            openrouter_headers: std::collections::BTreeMap::new(),
            meshy_headers: std::collections::BTreeMap::new(),
            bedrock_headers: std::collections::BTreeMap::new(),
            xai_headers: std::collections::BTreeMap::new(),
            meta_headers: std::collections::BTreeMap::new(),
            ollama_enabled: false,
            codex_enabled: false,
            codex_originator: None,
            codex_reasoning_effort: None,
            codex_verbosity: None,
            codex_replay_reasoning: default_true(),
            grok_enabled: false,
            file_uploads: default_true(),
            anthropic_cache_ttl: None,
            fallback_order: Vec::new(),
            provider_order: Vec::new(),
            zero_retention: false,
            zero_retention_agreements: Vec::new(),
        }
    }
}

/// The header names alone, for [`Debug`] output: a value is as often a
/// credential as not.
fn header_names(headers: &std::collections::BTreeMap<String, String>) -> Vec<&str> {
    headers.keys().map(String::as_str).collect()
}

/// `"<set>"` or `"<unset>"` for an optional secret, for [`Debug`] output.
pub(super) fn redacted(value: &Option<String>) -> &'static str {
    match value {
        Some(_) => "<set>",
        None => "<unset>",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `tracing::debug!(?config)` would otherwise put every provider key in
    /// the logs.
    #[test]
    fn debug_never_prints_the_keys() {
        let providers = ProviderConfig {
            anthropic_api_key: Some("sk-ant-SECRET-VALUE".to_string()),
            anthropic_headers: std::collections::BTreeMap::from([(
                "X-Gateway-Token".to_string(),
                "hdr-SECRET-VALUE".to_string(),
            )]),
            openai_api_key: Some("sk-openai-SECRET-VALUE".to_string()),
            google_api_key: Some("AIza-SECRET-VALUE".to_string()),
            codex_enabled: true,
            ..Default::default()
        };
        let rendered = format!("{providers:?}");
        assert!(!rendered.contains("SECRET-VALUE"), "key leaked: {rendered}");
        // "is it configured" is what a debug line is actually asking.
        assert!(rendered.contains("<set>"), "{rendered}");
        assert!(rendered.contains("codex_enabled: true"), "{rendered}");
        // A header's name says what is configured; its value is a credential
        // as often as not.
        assert!(rendered.contains("X-Gateway-Token"), "{rendered}");

        let empty = format!("{:?}", ProviderConfig::default());
        assert!(empty.contains("<unset>"), "{empty}");
        assert!(!empty.contains("<set>"), "{empty}");
    }

    /// The hand-written `Default` is what an empty `[providers]` table reads
    /// as, flags that are on unless turned off included.
    #[test]
    fn the_default_is_what_an_empty_table_reads_as() {
        let read: ProviderConfig = toml::from_str("").expect("an empty table reads");
        assert!(read.codex_replay_reasoning && read.file_uploads);
        assert_eq!(
            toml::to_string(&read).expect("serializes"),
            toml::to_string(&ProviderConfig::default()).expect("serializes"),
        );
    }
}
