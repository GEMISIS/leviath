//! `[providers]` and per-model overrides: which endpoint a stage's model resolves
//! to, and the credentials for it.
//!
//! `ProviderConfig` hand-writes its `Debug` so an API key cannot reach a log
//! through a derived one - the redaction is the reason this is not a derive.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    /// Opt-in like the Claude Code one, and for a different reason: the
    /// credential is a browser sign-in rather than a key, so enabling it
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
    /// generalizes [`default_provider`](super::Config::default_provider) from a
    /// single front-runner into a full ordering; leave it empty and
    /// `default_provider` alone decides, exactly as before.
    ///
    /// Naming a provider here is also a deliberate choice to route bare names
    /// through it, so a subscription transport (Codex, Claude Code) that is
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
            .field("claude_code_enabled", &self.claude_code_enabled)
            .field("claude_code_binary", &self.claude_code_binary)
            .field("claude_code_effort", &self.claude_code_effort)
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
            claude_code_enabled: false,
            claude_code_binary: None,
            claude_code_effort: None,
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
    /// OpenAI's own API (the Responses route) at another host, under a name
    /// of its own: an Azure resource or an API gateway in front of OpenAI.
    /// Several can sit side by side, each with its own key.
    Openai,
}

impl ModelProviderKind {
    /// The spelling the config file uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::OpenaiCompatible => "openai-compatible",
            Self::Openai => "openai",
        }
    }

    /// The kind a config file spelling names, if it is one.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "script" => Some(Self::Script),
            "openai-compatible" => Some(Self::OpenaiCompatible),
            "openai" => Some(Self::Openai),
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
    /// that pins it, which leaves a local model unreachable however the machine
    /// sets `default_provider`.
    /// `None` when the file does not mention it, `Some` when it does - including
    /// `Some(vec![])` for an explicit `serves = []`.
    ///
    /// The two are different states, and collapsing them into one empty `Vec`
    /// makes a stale `serves = []` unremovable: a save-back writes whatever the
    /// field holds, and an empty `Vec` writes as `serves = []` however it got
    /// there. `None` writes nothing, which is what lets the
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
    "retention",
    "zero_retention_request",
    "auth_header",
];

/// The keys an endpoint entry reads off `extra`, where a `flatten` puts them:
/// what the host keeps of its requests, which built-in provider's
/// zero-retention request field it takes, and the header its key goes in.
const ENDPOINT_EXTRA_KEYS: &[&str] = &["retention", "zero_retention_request", "auth_header"];

impl ModelProviderConfig {
    /// The kind this entry is, with absent read as a script.
    pub fn kind(&self) -> ModelProviderKind {
        self.kind.unwrap_or_default()
    }

    /// Whether this entry is a host Leviath reaches natively (either
    /// OpenAI-shaped kind) rather than a script.
    pub fn is_endpoint(&self) -> bool {
        self.kind() != ModelProviderKind::Script
    }

    /// Whether this entry is OpenAI's own API at a host of its own.
    pub fn is_openai(&self) -> bool {
        self.kind() == ModelProviderKind::Openai
    }

    /// The header the key goes in instead of `Authorization: Bearer`, when
    /// the entry names one (`api-key` for an Azure key, or
    /// `Ocp-Apim-Subscription-Key` for an API Management gateway).
    pub fn auth_header(&self) -> Option<String> {
        self.extra
            .get("auth_header")
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    }

    /// What is wrong with this entry, if anything, named against `name`.
    ///
    /// Checked at config load rather than at the first inference: an endpoint
    /// with nowhere to send a request is a config that cannot work, and the
    /// message should name the table to fix while the file is still in front
    /// of the person who wrote it.
    pub fn validate(&self, name: &str) -> anyhow::Result<()> {
        let kind = self.kind().as_str();
        if self.is_openai()
            && self
                .base_url
                .as_deref()
                .is_none_or(|url| url.trim().is_empty())
        {
            anyhow::bail!(
                "[model_providers.{name}] has kind = \"openai\" but no base_url; set \
                 base_url to the host's API root, such as \
                 \"https://<resource>.openai.azure.com/openai/v1\""
            );
        }
        if self.is_openai()
            && self
                .api_key
                .as_deref()
                .is_none_or(|key| key.trim().is_empty())
        {
            anyhow::bail!(
                "[model_providers.{name}] has kind = \"openai\" but no api_key; set the \
                 key the host issued (add auth_header = \"api-key\" when the host wants \
                 it in that header rather than as a bearer token)"
            );
        }
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
        if self.is_endpoint()
            && self.extra.contains_key("auth_header")
            && self.auth_header().is_none()
        {
            anyhow::bail!(
                "[model_providers.{name}] auth_header must name a header, such as \"api-key\""
            );
        }
        // `extra` exists to reach a script's `initialize`. An endpoint has no
        // script, so a key landing there is one the endpoint will never read:
        // `modles` leaves it with no catalogue and `heaeders` sends nothing,
        // and both would otherwise load clean. `Config::unknown_config_keys`
        // cannot catch them either, because `flatten` writes them straight
        // back.
        let mut keys: Vec<&str> = self
            .extra
            .keys()
            .map(String::as_str)
            .filter(|key| !ENDPOINT_EXTRA_KEYS.contains(key))
            .collect();
        if self.is_endpoint() && !keys.is_empty() {
            keys.sort_unstable();
            anyhow::bail!(
                "[model_providers.{name}] has kind = \"{kind}\" and \
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
