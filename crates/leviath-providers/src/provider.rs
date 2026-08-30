//! Provider trait and common types.

// Re-exported so `use crate::provider::*` keeps working: the types moved for
// structural reasons (the 1200-line rule), not as an interface change.
pub use crate::capabilities::{LimitsSource, ModelCapabilities, ModelCapabilityOverride};
pub use crate::failure::FailureKind;
use async_trait::async_trait;
use futures_core::Stream;
use leviath_net::read_caps::{BodyReadError, JSON_BODY_CAP, read_body_capped, read_text_capped};
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use thiserror::Error;

/// Result type for provider operations.
pub type Result<T> = std::result::Result<T, ProviderError>;

/// Why a provider cannot serve requests until someone intervenes.
///
/// These are the failures that no amount of retrying, waiting, or falling back
/// to a smaller request will fix: the account is out of money, or the key is
/// wrong. Keeping them apart from a generic [`ProviderError::ApiError`] is what
/// lets the runtime fail over to another provider and trip a circuit breaker
/// instead of killing every run with a raw JSON blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    /// The account is out of credits, or the request costs more than is left.
    CreditsExhausted,
    /// The API key is missing, malformed, or revoked.
    AuthFailed,
    /// The key authenticated but is not allowed to do this.
    Forbidden,
    /// The provider could not be reached at all: connection refused, DNS
    /// failure, TLS failure, or a timeout that outlived every retry.
    ///
    /// Unlike the three above, this is not something the provider told us - it
    /// is the absence of the provider. It still belongs here because the
    /// consequence is identical: the next request will fail the same way, so
    /// the run should move to a different provider rather than die.
    Unreachable,
}

impl UnavailableReason {
    /// A short, stable label for logs, metrics attributes, and `lev ps`.
    pub fn label(self) -> &'static str {
        match self {
            UnavailableReason::CreditsExhausted => "credits-exhausted",
            UnavailableReason::AuthFailed => "auth-failed",
            UnavailableReason::Forbidden => "forbidden",
            UnavailableReason::Unreachable => "unreachable",
        }
    }

    /// What the operator should actually do about it.
    fn remedy(self) -> &'static str {
        match self {
            UnavailableReason::CreditsExhausted => {
                "out of credits: top up the account, or lower max_tokens so the \
                 request costs less"
            }
            UnavailableReason::AuthFailed => {
                "the API key was rejected: check it with `lev auth status` and \
                 re-enter it with `lev setup`"
            }
            UnavailableReason::Forbidden => {
                "the API key is not allowed to use this model: check the \
                 account's plan and model permissions"
            }
            UnavailableReason::Unreachable => {
                "the provider could not be reached: check the network and the \
                 base URL, and whether a local server (ollama) is running"
            }
        }
    }

    /// Classify an HTTP failure as a provider-fatal one, or `None` if it is an
    /// ordinary error that says nothing about the provider's usability.
    ///
    /// Status alone is not enough. Anthropic reports a drained balance as a
    /// **400** whose body says "credit balance is too low", so a status-only
    /// check would read it as a malformed request and kill the run rather than
    /// failing over. The body scan catches that and its cousins across
    /// providers; every phrase is specific enough that a prompt quoting it
    /// verbatim is not a realistic concern (the body here is the provider's own
    /// error envelope, not the conversation).
    pub fn classify(status: u16, body: &str) -> Option<Self> {
        match status {
            402 => return Some(UnavailableReason::CreditsExhausted),
            401 => return Some(UnavailableReason::AuthFailed),
            403 => return Some(UnavailableReason::Forbidden),
            _ => {}
        }
        let b = body.to_ascii_lowercase();
        [
            "credit balance is too low",
            "insufficient credits",
            "requires more credits",
            "insufficient_quota",
            "exceeded your current quota",
            "billing",
        ]
        .iter()
        .any(|s| b.contains(s))
        .then_some(UnavailableReason::CreditsExhausted)
    }

    /// Classify a formatted error *message*, for the paths that never see the
    /// status code on its own.
    ///
    /// A Rhai script provider throws `#{ kind: "api", message: "HTTP 402 ..." }`
    /// and hands us the whole thing as one string, so the status has to be read
    /// back out of it. A script talking to an OpenAI-compatible endpoint must
    /// fail over and trip the breaker exactly as the built-in providers do.
    pub fn from_message(message: &str) -> Option<Self> {
        Self::classify(leading_http_status(message).unwrap_or(0), message)
    }
}

/// The status code in an `HTTP <code>` prefix, if the message carries one.
///
/// Anchored on the `HTTP ` marker that every provider in this crate formats,
/// rather than the first number anywhere in the text: an error body quoting a
/// token count must not be mistaken for a status.
fn leading_http_status(message: &str) -> Option<u16> {
    // A `[kind]` prefix may sit in front of the status now that a failure
    // carries what it was. Stepping over it keeps a message classifiable
    // whichever layer labelled it: a prefix that hid the status here would have
    // silently stopped a 402 from failing over, which is the behaviour the
    // status extraction exists to drive.
    let message = match message.strip_prefix('[') {
        Some(rest) => rest.split_once("] ").map_or(message, |(_, after)| after),
        None => message,
    };
    let rest = message.strip_prefix("HTTP ")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Errors that can occur during provider operations.
#[derive(Error, Debug)]
pub enum ProviderError {
    /// HTTP request failed. The string carries the kind as a `[label]` prefix
    /// when the caller knew it - see [`ProviderError::transport`].
    #[error("Request failed: {0}")]
    RequestFailed(String),

    /// API returned an error
    #[error("API error: {0}")]
    ApiError(String),

    /// The outbound HTTP client could not be constructed, so this provider
    /// cannot make any request at all.
    ///
    /// This is the *client* side: the workspace builds `reqwest` with `rustls`
    /// and `rustls-native-certs`, so constructing a client reads the machine's
    /// root certificate store in order to speak HTTPS to a provider API. A
    /// failure here means that store could not be read, not that any particular
    /// request was rejected - and it has nothing to do with `lev serve`'s own
    /// `--tls-cert` / `--tls-key`, which are a separate, already-fallible path.
    ///
    /// Separate from [`ProviderError::RequestFailed`] because no retry, backoff
    /// or change of model affects it; the remedy is on the host.
    #[error(
        "outbound HTTPS client could not be built ({0}); leviath reads the system root \
         certificate store to reach provider APIs"
    )]
    ClientBuild(String),

    /// The provider cannot serve any request until someone intervenes - the
    /// account is out of credits, or the key is bad. `detail` keeps the raw
    /// provider response for the logs; the message leads with what to do.
    #[error("{} ({detail})", .reason.remedy())]
    Unavailable {
        /// Which kind of intervention is needed, which is what decides the
        /// remedy text a user sees.
        reason: UnavailableReason,
        /// The provider's own response, kept for the logs. Not shown first: it
        /// is usually less actionable than the remedy.
        detail: String,
    },

    /// Rate limit exceeded
    #[error("Rate limit exceeded")]
    RateLimitExceeded {
        /// What the provider's `Retry-After` header asked for, in seconds, when
        /// it sent one. Carried out of the provider layer so the retry loop can
        /// wait as long as the server said instead of guessing.
        retry_after_secs: Option<u64>,
    },

    /// Invalid response from provider
    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    /// Token limit exceeded
    #[error("Token limit exceeded: {used} > {max}")]
    TokenLimitExceeded {
        /// Tokens the request would have sent.
        used: usize,
        /// The model's context limit.
        max: usize,
    },

    /// Other error
    #[error("{0}")]
    Other(String),
}

/// The message fragments that mean "the provider has no capacity right now":
/// a 429 rate limit, or Anthropic's 529 "overloaded".
///
/// Kept apart from [`SERVER_ERROR_SIGNALS`] because the two deserve different
/// waits. A 500 or a dropped connection is a blip that a second or two clears;
/// a capacity refusal is a window that lasts minutes, and retrying it on
/// blip-sized backoff just spends the attempts without ever leaving the
/// window.
const CAPACITY_SIGNALS: [&str; 5] = [
    // Rate limiting (a provider that maps 429 to ApiError rather than
    // RateLimitExceeded, e.g. Ollama).
    "429",
    "too many requests",
    "rate limit",
    // Anthropic returns 529 "overloaded" when the model is saturated.
    "529",
    "overloaded",
];

/// The message fragments that mean the server failed on its own account: an
/// ordinary 5xx, which is usually gone by the next attempt.
const SERVER_ERROR_SIGNALS: [&str; 8] = [
    "500",
    "502",
    "503",
    "504",
    "internal server error",
    "bad gateway",
    "service unavailable",
    "gateway timeout",
];

/// Whether a lowercased error message contains any of `signals`.
fn mentions_any(message: &str, signals: &[&str]) -> bool {
    signals.iter().any(|s| message.contains(s))
}

/// What the retry loop should do about a failed attempt, beyond the yes/no of
/// [`ProviderError::is_transient`].
///
/// The provider layer is the only place that knows whether a failure was the
/// provider running out of capacity and whether the server said when to come
/// back, and neither survives being flattened into an error string. Carrying
/// both here is what lets the dispatch layer honor a `Retry-After` and back off
/// on a capacity-sized schedule rather than a blip-sized one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetryAdvice {
    /// Whether the provider refused for want of capacity (429, or a 529
    /// "overloaded"), as opposed to failing for its own reasons.
    pub capacity: bool,
    /// How many seconds the provider's `Retry-After` header asked the caller to
    /// wait. `None` when it sent no hint, which is the usual case for a 529.
    pub retry_after_secs: Option<u64>,
}

impl ProviderError {
    /// What to do about this failure if it is retried: see [`RetryAdvice`].
    ///
    /// Only a capacity refusal ever carries advice. Everything else answers
    /// with the default, which asks for the ordinary schedule.
    pub fn retry_advice(&self) -> RetryAdvice {
        match self {
            ProviderError::RateLimitExceeded { retry_after_secs } => RetryAdvice {
                capacity: true,
                retry_after_secs: *retry_after_secs,
            },
            // A provider that reports 429/529 as a plain API error (Ollama, and
            // Anthropic's 529) leaves only the message to read. The header is
            // gone by then, so these get the capacity schedule with no hint.
            ProviderError::ApiError(msg) => RetryAdvice {
                capacity: mentions_any(&msg.to_ascii_lowercase(), &CAPACITY_SIGNALS),
                retry_after_secs: None,
            },
            _ => RetryAdvice::default(),
        }
    }

    /// Whether this failure is worth retrying - a transient network or
    /// server-side issue (connection reset, timeout, 429, 5xx / "overloaded") -
    /// as opposed to a permanent one (auth, invalid request, token limit)
    /// that would just fail again.
    pub fn is_transient(&self) -> bool {
        match self {
            // Network-level failures: connection reset, timeout, DNS, TLS.
            ProviderError::RequestFailed(_) => true,
            // 429 - back off and retry.
            ProviderError::RateLimitExceeded { .. } => true,
            // We only have the message, so match the common capacity and
            // server-side (5xx) signals - including Anthropic's 529
            // "overloaded". 4xx client errors carry none of these and stay
            // permanent.
            ProviderError::ApiError(msg) => {
                let m = msg.to_ascii_lowercase();
                mentions_any(&m, &CAPACITY_SIGNALS) || mentions_any(&m, &SERVER_ERROR_SIGNALS)
            }
            // A malformed response, an over-limit request, an unusable
            // provider, or an unknown error won't be fixed by retrying.
            ProviderError::InvalidResponse(_)
            | ProviderError::TokenLimitExceeded { .. }
            | ProviderError::Unavailable { .. }
            // The machine could not build an HTTPS client. Every retry does the
            // same work and reads the same certificate store, so it fails the
            // same way - and so would every other provider.
            | ProviderError::ClientBuild(_)
            | ProviderError::Other(_) => false,
        }
    }

    /// Whether this failure means the *provider itself* is unusable, rather
    /// than this one request being bad.
    ///
    /// The runtime uses this to decide whether to fail over to the next
    /// configured provider and to count the failure against that provider's
    /// circuit breaker. A bad request or a malformed response says nothing
    /// about the provider, so [`ProviderError::Unavailable`] and a transport
    /// failure are the only two that qualify.
    pub fn unavailable_reason(&self) -> Option<UnavailableReason> {
        match self {
            ProviderError::Unavailable { reason, .. } => Some(*reason),
            // A provider we could not open a connection to has told us nothing
            // about this request and everything about itself. The retry policy
            // has already tried four times with backoff by the time one of
            // these surfaces, so the next attempt belongs somewhere else.
            //
            // This is what broke an OpenRouter-only install: `ollama` is
            // registered whether or not a server is running, every bundled
            // blueprint lists it, and a refused connection to localhost:11434
            // counted as an ordinary error - so the run died at iteration 0
            // with a usable OpenRouter fallback sitting untouched behind it.
            ProviderError::RequestFailed(_) => Some(UnavailableReason::Unreachable),
            _ => None,
        }
    }
}

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
}

impl ModelInfo {
    /// An entry from a compiled table: nothing learned, nothing dated.
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
        }
    }

    /// This entry with a display name.
    pub fn named(mut self, display_name: Option<String>) -> Self {
        self.display_name = display_name;
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

/// Rich message content: either a plain text string or structured content blocks.
///
/// Provider serialization converts this to the appropriate API format
/// (e.g., Anthropic content blocks, OpenAI message + tool_calls).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// Plain text content (backward compatible).
    Text(String),
    /// Structured content blocks (tool_use, tool_result, text).
    Blocks(Vec<ContentBlock>),
}

impl From<String> for MessageContent {
    fn from(s: String) -> Self {
        MessageContent::Text(s)
    }
}

impl From<&str> for MessageContent {
    fn from(s: &str) -> Self {
        MessageContent::Text(s.to_string())
    }
}

impl MessageContent {
    /// Get the plain text content, concatenating text blocks if needed.
    pub fn as_text(&self) -> String {
        match self {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

/// A content block within a rich message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// A text content block.
    #[serde(rename = "text")]
    Text {
        /// The text itself.
        text: String,
    },
    /// A tool use request from the assistant.
    #[serde(rename = "tool_use")]
    ToolUse {
        /// Provider-assigned call id, which the matching result must quote back.
        id: String,
        /// The tool the model asked for.
        name: String,
        /// Arguments as the model supplied them, before any validation.
        input: serde_json::Value,
        /// See [`ToolCall::thought_signature`]: replayed verbatim so a
        /// provider that requires it accepts the follow-up request.
        ///
        /// **Never serialized.** This is one provider's field riding in shared
        /// history, and history is replayed to whichever provider runs next -
        /// which, with per-stage models, is routinely a different one. Anthropic
        /// rejects the unknown key outright (`tool_use.thought_signature: Extra
        /// inputs are not permitted`), so a Gemini stage followed by an
        /// Anthropic stage dies on its first request.
        ///
        /// A provider that wants it emits it deliberately rather than getting
        /// it by default: `openai_compat` already does exactly that when
        /// building its tool calls, which is why the OpenAI-shaped path
        /// (Gemini included) keeps working. The field stays on the struct - it
        /// is still needed in memory and still persisted through
        /// `SerializedToolCall` - it just never reaches a body nobody asked to
        /// put it in.
        #[serde(default, skip_serializing)]
        thought_signature: Option<String>,
    },
    /// A tool result from executing a tool.
    #[serde(rename = "tool_result")]
    ToolResult {
        /// The [`ContentBlock::ToolUse`] id this answers.
        tool_use_id: String,
        /// The tool's output as text. Every provider takes a string here, so a
        /// structured result is already rendered by this point.
        content: String,
        /// Whether the tool refused or failed.
        is_error: bool,
    },
}

/// A system prompt block, separated from conversation messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemBlock {
    /// The text content of this system block.
    pub text: String,
    /// Cache hint for this system block.
    pub cache_hint: leviath_core::CacheHint,
    /// The region this block was rendered from, for diagnostics.
    ///
    /// Empty for a block that is not a region - a hint, a tool preamble - which
    /// is exactly the set of blocks no volatility warning could be about.
    ///
    /// Defaulted on the wire so a request serialized before these fields
    /// existed still deserializes - a script provider that round-trips one, or
    /// a dumped body replayed later, must not fail on a field it predates.
    #[serde(default)]
    pub region: String,
    /// How much the region this block came from moves between requests.
    ///
    /// Carried so assembly can order blocks by it: a provider caches by prefix,
    /// so a block that moves invalidates everything behind it, and the
    /// arrangement that pays is stable content first and churn last. The
    /// region's *kind* cannot answer this - a pinned region is written
    /// constantly - so the blueprint says and this carries the answer.
    #[serde(default)]
    pub volatility: leviath_core::Volatility,
}

/// An `f32` as JSON, at the precision it was written with.
///
/// `serde_json` widens an `f32` to `f64` to store it, and `0.7f32` widened is
/// `0.699999988079071`. That is what every request carried: it read as a
/// Leviath bug in provider error messages, and Z.AI rejects it outright with
/// `The temperature parameter is illegal: 限制小数点[2]位` - at most two decimal
/// places - which made an entire vendor family unusable.
///
/// `f32`'s own `Display` gives the shortest decimal that round-trips back to
/// the same `f32`, so `0.7f32` prints "0.7". Parsing that as `f64` gets the
/// number the blueprint author actually wrote, without imposing a fixed
/// precision on someone who wanted `0.125`.
pub(crate) fn json_number(value: f32) -> serde_json::Value {
    // `f32::Display` always produces a decimal that parses back, including for
    // the non-finite values, so the fallback is the same number rather than a
    // branch nothing reaches.
    serde_json::json!(value.to_string().parse::<f64>().unwrap_or(f64::from(value)))
}

/// Request for LLM inference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceRequest {
    /// System prompt blocks, separate from conversation messages.
    /// Providers that support a dedicated system prompt (Anthropic, OpenAI)
    /// will serialize these appropriately. Defaults to empty.
    #[serde(default)]
    pub system: Vec<SystemBlock>,

    /// The prompt or messages to send
    pub messages: Vec<Message>,

    /// Model to use
    pub model: String,

    /// Maximum tokens to generate
    pub max_tokens: usize,

    /// Temperature for sampling
    pub temperature: f32,

    /// Available tools
    pub tools: Vec<Tool>,

    /// Additional provider-specific parameters
    pub extra: serde_json::Value,

    /// Optional per-call wall-clock deadline in seconds. When set, providers
    /// bound this specific call to it (HTTP: a per-request total timeout;
    /// claude-code: its subprocess timeout). When `None`, the provider's default
    /// applies (see `DEFAULT_INFERENCE_TIMEOUT_SECS`). Sourced from a stage's
    /// `[stages.<name>.model] request_timeout_secs`.
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,
}

/// A message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Role (system, user, assistant)
    pub role: String,

    /// Message content - plain text or structured content blocks.
    pub content: MessageContent,

    /// If true, this message is a cache breakpoint - the provider should
    /// mark everything up to and including this message as cacheable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cache_breakpoint: bool,

    /// An opaque provider token carried with an assistant turn, to be replayed
    /// by the provider that issued it.
    ///
    /// **Never serialized.** This is one provider's field riding in shared
    /// history, and history is replayed to whichever provider runs the next
    /// stage - routinely a different one, since models are chosen per stage.
    /// A provider that wants it emits it deliberately, exactly as
    /// `openai_compat` does for [`ContentBlock::ToolUse::thought_signature`];
    /// the alternative is a stage handoff that dies on an unknown key.
    #[serde(default, skip_serializing)]
    pub reasoning: Option<String>,
}

/// A tool that can be called by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    /// Tool name
    pub name: String,

    /// Tool description
    pub description: String,

    /// JSON schema for tool parameters
    pub parameters: serde_json::Value,
}

/// Response from LLM inference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResponse {
    /// The model's response text
    pub content: String,

    /// Tool calls requested by the model
    pub tool_calls: Vec<ToolCall>,

    /// Tokens used (prompt + completion)
    pub tokens_used: TokenUsage,

    /// Whether the response was complete or truncated
    pub finish_reason: FinishReason,

    /// An opaque provider token to replay with this turn, when the provider
    /// issued one.
    ///
    /// A stateless backend keeps no server-side thread, so the model's chain of
    /// thought survives into the next turn only if the same sealed blob is
    /// handed back. Stored on the assistant turn and replayed by the provider
    /// that produced it, never by another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

// `TokenUsage` lives in `crate::pricing` alongside the rates it is priced
// at, and is re-exported here because every provider imports it from this
// module and the move was structural, not an interface change.
pub use crate::pricing::TokenUsage;

/// Reason inference completed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FinishReason {
    /// Normal completion
    Complete,

    /// Hit token limit
    TokenLimit,

    /// Model requested tool call
    ToolCall,

    /// Model requested stop
    Stop,

    /// The provider gave a reason this build does not recognise. Kept apart
    /// from [`FinishReason::Complete`] so a new way of stopping (a content
    /// filter, a gateway's own error marker) is visible in the journal rather
    /// than passing as a finished answer.
    Unknown,
}

impl PartialEq for FinishReason {
    #[inline(never)]
    fn eq(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

/// A tool call from the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Tool call ID
    pub id: String,

    /// Tool name
    pub name: String,

    /// Tool arguments
    pub arguments: serde_json::Value,

    /// Opaque provider token that must be echoed back with this call on the
    /// next request. Gemini 3.x returns a `thought_signature` per function
    /// call and rejects a follow-up that omits it, so the value has to survive
    /// the round trip through the context window. `None` for providers that
    /// have no such requirement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

/// A chunk from a streaming inference response.
#[derive(Debug, Clone)]
pub struct StreamChunk {
    /// Text delta
    pub delta: String,

    /// Partial tool call updates
    pub tool_calls: Vec<ToolCallDelta>,

    /// Token usage (usually only on final chunk)
    pub tokens: Option<TokenUsage>,

    /// Finish reason (only on final chunk)
    pub finish_reason: Option<FinishReason>,

    /// See [`InferenceResponse::reasoning`]. Arrives on whichever chunk
    /// carries the provider's reasoning item, not necessarily the last.
    pub reasoning: Option<String>,
}

/// A partial tool call update from streaming.
#[derive(Debug, Clone)]
pub struct ToolCallDelta {
    /// Index of the tool call being built
    pub index: usize,

    /// Tool call ID (sent on first delta for this index)
    pub id: Option<String>,

    /// Tool name (sent on first delta for this index)
    pub name: Option<String>,

    /// Partial arguments JSON string
    pub arguments_delta: String,

    /// See [`ToolCall::thought_signature`]. Sent on the delta that opens this
    /// index, like the id and the name.
    ///
    /// Streaming had no way to carry one, which was survivable only while
    /// nothing streamed: Gemini 3.x refuses a function call replayed without
    /// the signature it issued, so a streamed tool call would have been
    /// rejected on the very next turn.
    pub thought_signature: Option<String>,
}

/// Rate limit configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Maximum requests per minute
    pub requests_per_minute: u32,

    /// Maximum tokens per minute
    pub tokens_per_minute: u32,
}

// Two subjects lifted out of this module, both because they are their own
// subject and both because the module is at the structure limit without them.
// Re-exported flat, so `provider::build_http_client` and every other existing
// path still resolves and the split stays a pure move.
mod http;
pub use http::{
    DEFAULT_INFERENCE_TIMEOUT_SECS, HttpClient, HttpClientFactory, HttpError,
    SIDE_CALL_TIMEOUT_SECS, apply_request_timeout, build_http_client, build_http1_client,
    malformed_url_error, side_call_client,
};

// Folding a streamed answer back into one response.
pub(crate) mod stream;
pub use stream::collect_stream;

/// Trait for LLM providers.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Execute inference with the given request.
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse>;

    /// Execute streaming inference with the given request.
    ///
    /// Returns a stream of chunks that can be consumed incrementally.
    /// Default implementation collects the full response from `infer()`.
    async fn infer_stream(
        &self,
        request: &InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        let response = self.infer(request).await?;
        let chunk = StreamChunk {
            delta: response.content,
            tool_calls: response
                .tool_calls
                .iter()
                .enumerate()
                .map(|(i, tc)| ToolCallDelta {
                    index: i,
                    id: Some(tc.id.clone()),
                    name: Some(tc.name.clone()),
                    arguments_delta: tc.arguments.to_string(),
                    thought_signature: tc.thought_signature.clone(),
                })
                .collect(),
            tokens: Some(response.tokens_used),
            finish_reason: Some(response.finish_reason),
            reasoning: None,
        };
        Ok(Box::pin(stream_once::once(Ok(chunk))))
    }

    /// Count tokens in the given text for this provider's models.
    ///
    /// Providers with an exact remote token-count endpoint (Anthropic, Gemini)
    /// call it here and fall back to a local heuristic on any error, so this is
    /// infallible. Providers without such an endpoint (OpenAI via tiktoken,
    /// OpenRouter, Ollama, Claude Code) compute locally and never `.await` on
    /// the network. Because an implementation may perform a network round-trip,
    /// do **not** call this on a hot per-entry accounting path; use it for
    /// bounded, request-level checks.
    async fn count_tokens(&self, text: &str, model: &str) -> usize;

    /// Get the maximum context tokens for a model.
    fn max_context_tokens(&self, model: &str) -> usize;

    /// Get the provider name.
    fn name(&self) -> &str;

    /// Get the capabilities of the given model.
    fn capabilities(&self, model: &str) -> ModelCapabilities;

    /// Learn what this provider's own API says about its models, before any
    /// inference asks.
    ///
    /// [`Self::capabilities`] is synchronous and called on the inference path,
    /// so it cannot go and ask. A provider whose real answer lives behind a
    /// network call needs somewhere to fetch it once, and this is that
    /// somewhere: the daemon awaits it at start-up, beside the other warm-up
    /// steps, so the first run already has the answer rather than racing it.
    ///
    /// Providers whose capability table is compiled in need nothing here, which
    /// is why this defaults to doing nothing. Failure is the caller's to
    /// tolerate: a provider that cannot reach its own API should degrade to its
    /// built-in table, not stop a daemon from starting.
    async fn prime_capabilities(&self) -> Result<()> {
        Ok(())
    }

    /// Get `models` ready, just before a run that intends to use them starts.
    ///
    /// `models` is every model the run's blueprint names, bare (no provider
    /// prefix) and deduplicated. A provider takes the ones it serves and ignores
    /// the rest - the caller does not know which are whose, and asking each
    /// provider is how a model named without a provider still reaches the one
    /// that has it.
    ///
    /// Defaults to doing nothing, which is right for every provider whose models
    /// are always ready. It exists for the ones where "ready" is a state the
    /// machine has to be put into: Ollama serves a model out of memory and can
    /// only report the window it truly allocated for one it has resident, so a
    /// run whose first inference loads the model has already sized its context
    /// regions against a guess by then. Percentage budgets resolve once, at
    /// spawn, into absolute numbers - so that guess is not corrected later, it
    /// is what the whole run uses.
    ///
    /// Called before the run is built, and awaited, because arriving after the
    /// spawn would be arriving after the only moment it could have helped. It is
    /// bounded by the caller and its failures are warnings: a run that could not
    /// be warmed still runs, on whatever the compiled table says.
    async fn warm_models(&self, models: &[String]) -> Result<()> {
        let _ = models;
        Ok(())
    }

    /// List models available from this provider.
    ///
    /// Returns an empty list by default; providers may override to enumerate
    /// their available models.
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    /// This provider's id for `model_key`, or `None` if it does not serve it.
    ///
    /// A blueprint names a model; the route to it belongs to the machine, so
    /// asking each provider lets an author stop enumerating routes they cannot
    /// know. `model_key` carries no vendor prefix (`gpt-5.5`), because the same
    /// model is spelled `openai/gpt-5.5` through a gateway; the answer is
    /// whatever THIS provider wants in a request.
    ///
    /// Defaults to whether [`Self::capabilities`] is a real entry rather than
    /// the fallback. A gateway fronting hundreds of models overrides it.
    fn serves_model(&self, model_key: &str) -> Option<String> {
        self.serves_model_from_table(model_key)
    }

    /// Every model id this provider will accept, when it is in a position to
    /// say so.
    ///
    /// The difference between this and [`Self::serves_model`] is what a `None`
    /// means. `serves_model` answers `None` both for a model a provider has
    /// definitely never heard of and for one it simply cannot check, and a
    /// caller holding the two together cannot refuse anything: refusing on
    /// "cannot check" would deny a model that works. So the two are separated
    /// here. `Some` is a **complete** list, and a name outside it is a name
    /// this provider will reject. `None` is "cannot say" and every caller
    /// treats it as "do not check", never as a refusal.
    ///
    /// Defaults to `None`, which is the right answer for a provider whose
    /// catalogue is open (Ollama serves whatever has been pulled, and a name
    /// missing from that is a model nobody has fetched yet rather than a model
    /// that does not exist), and for one whose catalogue this build only knows
    /// from a compiled-in table that may be older than the API.
    fn served_catalog(&self) -> Option<Vec<String>> {
        None
    }

    /// Whether this provider may only be reached by name.
    ///
    /// A blueprint entry that names a model without a provider asks every
    /// registered provider whether it serves that name, and the first one that
    /// says yes wins. That is right for providers a user configured to be
    /// interchangeable, and wrong for one whose selection changes what gets
    /// billed: enabling a subscription transport must not silently re-route
    /// existing bare-named stages onto the subscription.
    ///
    /// A provider answering `true` is still reachable by an explicit
    /// `provider/model` reference, an explicit `fallback_order` entry, or by
    /// being the configured `default_provider`. It is only excluded from
    /// winning a route nobody asked it to serve.
    ///
    /// `false` by default, which is the behaviour every provider had before
    /// this existed.
    fn explicit_route_only(&self) -> bool {
        false
    }

    /// [`Self::serves_model`] answered from the compiled-in capability table.
    ///
    /// Split out so an override can fall back to it: a gateway answers from its
    /// live catalogue, and when that catalogue is empty (priming failed, or has
    /// not run yet) the table is a better answer than "no".
    fn serves_model_from_table(&self, model_key: &str) -> Option<String> {
        // Decided against this provider's own answer for a model it certainly
        // does not know, not against `ModelCapabilities::default()`. Those are
        // the same test only for a provider whose unknown-model answer IS the
        // default; one that falls back to a family-shaped guess differs from the
        // default for every string, so it claimed every model in existence.
        // Measured, that made `google` claim `claude-opus-5`, and this is what
        // decides where a bare model name resolves.
        let unknown = self.capabilities("\u{0}no-such-model\u{0}");
        (self.capabilities(model_key) != unknown).then(|| model_key.to_string())
    }

    /// What this provider charges for `model`, or `None` when it does not know.
    ///
    /// The peer of [`Self::capabilities`], and fed the same way: a provider
    /// whose rates live behind its API fetches them in
    /// [`Self::prime_capabilities`] and answers from the primed table here,
    /// because this is called on the accounting path and must not go to the
    /// network.
    ///
    /// **`None` is the safe answer and the default.** An unpriced call makes
    /// its run's cost report `UNKNOWN` rather than contributing zero, so a
    /// provider that has not implemented this yet cannot cause a total to
    /// silently understate. That is the property worth protecting: a partial
    /// total looks authoritative and gets quoted onward.
    ///
    /// Prefer reporting real cost over rates where the API offers it - see
    /// [`TokenUsage::reported_cost_usd`], which is used in preference to
    /// anything computed here.
    fn pricing(&self, _model: &str) -> Option<crate::pricing::ModelPricing> {
        None
    }
}

// ─── Shared provider helpers ─────────────────────────────────────────────────

/// Map an OpenAI-style `finish_reason` string to a `FinishReason`.
///
/// Used by both the OpenAI and OpenRouter providers which share the same
/// Chat Completions API response schema.
pub(crate) fn parse_openai_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Complete,
        "tool_calls" => FinishReason::ToolCall,
        "length" => FinishReason::TokenLimit,
        other => {
            tracing::debug!(
                reason = other,
                "unrecognised finish_reason from the provider"
            );
            FinishReason::Unknown
        }
    }
}

/// Turn the argument text of a tool call into the value the runtime executes.
///
/// Empty text is a call with no arguments, and becomes `{}` so a tool that
/// takes none is still called. Text that is not JSON is **kept as text**
/// rather than replaced by `{}`: the usual reason it is not JSON is that the
/// reply hit its output cap mid-argument, and executing the tool with nothing
/// hid that from the model. It re-sent the same oversized call and was cut
/// off the same way, five times in a row, before the stage gave up. A string
/// where an object should be fails schema validation, and the runtime reads
/// the string shape as "this call was cut off" and says so back to the model.
pub(crate) fn parse_tool_arguments(raw: &str) -> serde_json::Value {
    let raw = raw.trim();
    if raw.is_empty() {
        return serde_json::Value::Object(serde_json::Map::new());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
}

/// Model names remembered for the life of the process.
///
/// What a provider learns about a model by being refused (no temperature,
/// no tools over a reasoning effort) or by warning about it once is worth
/// keeping across requests, and clones of the provider share the same set.
#[derive(Debug, Clone, Default)]
pub(crate) struct ModelMemo(std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>);

impl ModelMemo {
    /// Whether `model` has been recorded.
    pub(crate) fn contains(&self, model: &str) -> bool {
        leviath_core::sync::lock(&self.0).contains(model)
    }

    /// Record `model`; `true` the first time, `false` if it was already there.
    pub(crate) fn insert(&self, model: &str) -> bool {
        leviath_core::sync::lock(&self.0).insert(model.to_string())
    }

    /// How many models are recorded.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        leviath_core::sync::lock(&self.0).len()
    }

    /// Whether nothing has been recorded.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// How long a response's `Retry-After` header asks the caller to wait.
///
/// Only the delta-seconds form is read. The header's other form is an HTTP
/// date, which every provider API in use here answers with seconds instead, and
/// reading it would mean trusting the server's clock against ours; an
/// unparseable value is treated as no hint at all, which falls back to the
/// caller's own backoff.
pub(crate) fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// Check an HTTP response for errors and return it on success.
///
/// - On 429 (rate limit): notifies the optional rate limiter and returns `RateLimitExceeded`.
/// - On a provider-fatal failure (see [`UnavailableReason::classify`]): returns `Unavailable`.
/// - On any other non-2xx: reads the body and returns `ApiError`.
/// - On 2xx: returns `Ok(response)` so the caller can read the body.
///
/// Pass the full `reqwest::Response`; it is returned back on success.
pub(crate) async fn check_http_response(
    response: reqwest::Response,
    limiter: Option<&crate::rate_limit::RateLimiter>,
) -> Result<reqwest::Response> {
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        // Extract retry-after *before* consuming the response body.
        let retry_after = retry_after_secs(response.headers());
        if let Some(l) = limiter {
            l.handle_rate_limit(retry_after).await;
        }
        // The hint rides along on the error: the client-side limiter paces the
        // *next* request, while the dispatch layer's retry loop is what decides
        // how long this one waits before trying again.
        return Err(ProviderError::RateLimitExceeded {
            retry_after_secs: retry_after,
        });
    }
    if !status.is_success() {
        // Capped like a good body: an error page is a body too, and a gateway
        // that answers a 502 with its whole access log is still a peer.
        let error_body = read_text_capped(response, JSON_BODY_CAP)
            .await
            .unwrap_or_else(|e| e.to_string());
        // The kind rides in front of the status so it survives every layer that
        // only passes strings - including a Rhai script, which sees the message
        // and nothing else. A bare status left "their endpoint is down" and
        // "your base_url has a typo" reading identically.
        let kind = FailureKind::from_status(status.as_u16());
        let detail = format!(
            "[{}] HTTP {}: {} - {}",
            kind.label(),
            status,
            error_body,
            kind.remedy()
        );
        // An out-of-credits or bad-key response is worth telling apart: the
        // runtime fails over on it and counts it against the provider's
        // circuit breaker, where a plain `ApiError` would just kill the run.
        return Err(
            match UnavailableReason::classify(status.as_u16(), &error_body) {
                Some(reason) => ProviderError::Unavailable { reason, detail },
                None => ProviderError::ApiError(detail),
            },
        );
    }
    Ok(response)
}

/// Read a response body to completion and parse it as JSON.
///
/// The two halves fail for entirely different reasons and must not share an
/// error variant. Bytes that never arrived - a reset connection, a socket that
/// died while the machine was asleep, a truncated body - are a *transport*
/// failure: [`ProviderError::RequestFailed`], which is transient, gets retried,
/// counts against the provider's circuit breaker and is eligible for failover.
/// Bytes that arrived and did not fit the schema are the provider's own fault:
/// [`ProviderError::InvalidResponse`], which is permanent, because sending the
/// same request again produces the same unusable answer.
///
/// `reqwest`'s own `Response::json` collapses both into one `Decode` error whose
/// message is the famously unhelpful "error decoding response body". Routing
/// that through `InvalidResponse` made every network blip permanent: a run with
/// dozens of iterations of completed work died outright rather than retrying,
/// because a dead socket was being reported as malformed JSON. The streaming
/// path already drew this line correctly (see `openai_compat::stream_chat`);
/// this is the buffered path drawing the same one.
pub(crate) async fn decode_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T> {
    decode_json_capped(response, JSON_BODY_CAP).await
}

/// [`decode_json`] with the body cap as a parameter, so a test can hit the
/// cap with a few kilobytes rather than 64 MiB.
///
/// A body past the cap is [`ProviderError::InvalidResponse`]: the same
/// request would draw the same oversized answer, so retrying it only spends
/// the attempts, and the message names the cap and the peer.
pub(crate) async fn decode_json_capped<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    cap: usize,
) -> Result<T> {
    let bytes = read_body_capped(response, cap)
        .await
        .map_err(ProviderError::from)?;
    serde_json::from_slice(&bytes).map_err(|e| ProviderError::InvalidResponse(e.to_string()))
}

impl From<BodyReadError> for ProviderError {
    /// Bytes that never arrived are a transport failure (retried, counted
    /// against the breaker); a body past the cap is the provider's own fault
    /// and permanent, since the same request draws the same oversized answer.
    fn from(e: BodyReadError) -> Self {
        match e {
            BodyReadError::Transport(e) => {
                ProviderError::transport("reading the response body", &e)
            }
            too_large @ BodyReadError::TooLarge { .. } => {
                ProviderError::InvalidResponse(too_large.to_string())
            }
        }
    }
}

// Helper module for single-item streams
mod stream_once {
    use futures_core::Stream;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub struct Once<T> {
        item: Option<T>,
    }

    impl<T: Unpin> Stream for Once<T> {
        type Item = T;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.item.take())
        }
    }

    pub fn once<T>(item: T) -> Once<T> {
        Once { item: Some(item) }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    /// Most providers have nothing to get ready, which is what the default is
    /// for. Exercised through a real one rather than a stub: a stub written for
    /// this would need four other methods nothing calls, and those would be the
    /// untested code instead.
    #[tokio::test]
    async fn a_provider_that_needs_no_warming_accepts_the_call() {
        let anthropic = crate::AnthropicProvider::new(
            build_http_client(None).expect("a test client builds"),
            "sk-ant-test".to_string(),
        );

        anthropic
            .warm_models(&["claude-sonnet-5".to_string()])
            .await
            .expect("the default does nothing, successfully, and asks no network");
    }

    /// A temperature reaches the wire as the number that was written.
    ///
    /// `serde_json` stores an `f32` by widening it to `f64`, and `0.7f32`
    /// widened is `0.699999988079071`. Every request carried that. It reads as
    /// a Leviath bug in provider error text, and Z.AI rejects it outright -
    /// "The temperature parameter is illegal", at most two decimal places -
    /// which made the whole GLM family unusable.
    #[test]
    fn a_temperature_serializes_at_the_precision_it_was_written_with() {
        assert_eq!(json_number(0.7f32).to_string(), "0.7");
        assert_eq!(json_number(1.0f32).to_string(), "1.0");
        assert_eq!(json_number(0.0f32).to_string(), "0.0");
        // Someone who wanted three decimals keeps them: the shortest
        // round-tripping form is the number itself, not a rounded one.
        assert_eq!(json_number(0.125f32).to_string(), "0.125");

        // What the plain conversion produces, and what Z.AI refused.
        let widened = serde_json::json!(0.7f32);
        assert_eq!(widened.to_string(), "0.699999988079071");
        assert_ne!(json_number(0.7f32).to_string(), widened.to_string());
    }

    /// A non-finite value does not panic on the way through. Not a temperature
    /// any caller sends, but the conversion should survive one.
    #[test]
    fn a_non_finite_number_does_not_panic() {
        // serde_json has no NaN, so it lands as null - which is the same
        // answer it gave before, and not a crash.
        assert!(json_number(f32::NAN).is_null());
    }

    // ─── A partial [model_capabilities] entry corrects, it does not replace ──

    /// A model whose built-in answer is nothing like `Default`.
    fn base() -> ModelCapabilities {
        ModelCapabilities {
            supports_temperature: false,
            supports_streaming: false,
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 400_000,
            max_output_tokens: 64_000,
            limits_source: LimitsSource::Builtin,
        }
    }

    #[test]
    fn an_entry_naming_one_field_leaves_the_rest_alone() {
        // The reported case: correcting a wrong context window and nothing else.
        let over = ModelCapabilityOverride {
            max_context_tokens: Some(1_048_576),
            ..Default::default()
        };
        let merged = over.apply_to(base());
        assert_eq!(merged.max_context_tokens, 1_048_576);
        // Everything unmentioned is the provider's, not `Default`'s. Taking
        // `Default` here would have dropped a 64k output cap to 4096 and turned
        // two `false`s into `true`.
        assert_eq!(merged.max_output_tokens, 64_000);
        assert!(!merged.supports_temperature);
        assert!(!merged.supports_streaming);
    }

    #[test]
    fn an_entry_naming_nothing_changes_nothing() {
        let merged = ModelCapabilityOverride::default().apply_to(base());
        assert_eq!(merged.max_context_tokens, base().max_context_tokens);
        assert_eq!(merged.max_output_tokens, base().max_output_tokens);
        assert_eq!(merged.supports_tools, base().supports_tools);
    }

    #[test]
    fn every_field_is_individually_overridable() {
        // Each field on its own, so a typo in `apply_to` that read the wrong
        // one cannot hide behind a neighbour that happens to match.
        let full = ModelCapabilityOverride::from(ModelCapabilities {
            supports_temperature: true,
            supports_streaming: true,
            supports_tools: false,
            supports_system_prompt: false,
            max_context_tokens: 7,
            max_output_tokens: 9,
            limits_source: LimitsSource::Builtin,
        });
        let merged = full.apply_to(base());
        assert!(merged.supports_temperature);
        assert!(merged.supports_streaming);
        assert!(!merged.supports_tools);
        assert!(!merged.supports_system_prompt);
        assert_eq!(merged.max_context_tokens, 7);
        assert_eq!(merged.max_output_tokens, 9);
    }

    // The TOML shapes this type exists for - a one-field table, and a
    // misspelled key - are asserted where the config is actually loaded, in
    // `leviath-cli`'s `config` tests, rather than pulling a TOML parser into
    // this crate's dev-dependencies to say the same thing twice.

    // ─── redirect policy ──────────────────────────────────────────────────

    /// Serve `responses` in order, one per connection, on one address, and
    /// report how many requests arrived.
    ///
    /// The provider client sets `pool_max_idle_per_host(0)`, so each hop of a
    /// redirect chain opens a fresh connection - a one-shot mock cannot follow
    /// one.
    async fn serve_sequence(
        responses: Vec<String>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            for body in responses {
                let (mut socket, _) = listener.accept().await.expect("accept");
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(body.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{addr}"), hits)
    }

    fn redirect_to(location: &str) -> String {
        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n")
    }

    const OK_BODY: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";

    /// A redirect that stays on the host the key was meant for is ordinary and
    /// must keep working - some gateways answer that way.
    #[tokio::test]
    async fn a_same_origin_redirect_is_followed() {
        let (base, hits) = serve_sequence(vec![redirect_to("/second"), OK_BODY.to_string()]).await;
        let response = build_http_client(Some(10))
            .expect("an HTTPS client builds in tests")
            .get(format!("{base}/first"))
            .header("x-api-key", "super-secret")
            .send()
            .await
            .expect("a same-origin redirect should be followed");
        assert_eq!(response.status(), 200);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// reqwest strips `Authorization` across origins on its own, but not a
    /// custom header - and the provider keys travel as `x-api-key` and
    /// `x-goog-api-key`. Without a policy it would carry them to whatever a
    /// redirect named.
    #[tokio::test]
    async fn a_cross_origin_redirect_is_not_followed() {
        let (thief, thief_hits) = serve_sequence(vec![OK_BODY.to_string()]).await;
        let (base, _) = serve_sequence(vec![redirect_to(&format!("{thief}/steal"))]).await;

        let response = build_http_client(Some(10))
            .expect("an HTTPS client builds in tests")
            .get(format!("{base}/first"))
            .header("x-api-key", "super-secret")
            .send()
            .await
            .expect("stopping returns the 3xx rather than erroring");
        assert_eq!(
            response.status(),
            302,
            "the redirect is surfaced, not followed"
        );
        assert_eq!(
            thief_hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no request carrying the api key may reach another origin"
        );
    }

    #[test]
    fn malformed_url_error_yields_a_real_reqwest_error() {
        // The only way to obtain a `reqwest::Error` without I/O, and the reason
        // the client-build failure path is testable at all. If a future reqwest
        // starts accepting this input, the error paths that depend on it become
        // unreachable - so this asserts the trigger still triggers.
        let err = malformed_url_error();
        assert!(err.is_builder(), "expected a builder error, got {err:?}");
    }

    #[test]
    fn provider_error_is_transient_classification() {
        // Network + rate-limit + 5xx / overloaded ⇒ retry.
        assert!(ProviderError::RequestFailed("connection reset".into()).is_transient());
        assert!(
            ProviderError::RateLimitExceeded {
                retry_after_secs: None
            }
            .is_transient()
        );
        assert!(ProviderError::ApiError("HTTP 503 Service Unavailable".into()).is_transient());
        assert!(ProviderError::ApiError("HTTP 429 Too Many Requests".into()).is_transient());
        assert!(ProviderError::ApiError("rate limit exceeded".into()).is_transient());
        assert!(ProviderError::ApiError("500 internal server error".into()).is_transient());
        assert!(ProviderError::ApiError("529 overloaded".into()).is_transient());
        assert!(ProviderError::ApiError("502 Bad Gateway".into()).is_transient());
        assert!(ProviderError::ApiError("504 gateway timeout".into()).is_transient());
        // 4xx client errors + malformed / limit / unknown ⇒ permanent.
        assert!(!ProviderError::ApiError("401 unauthorized".into()).is_transient());
        assert!(!ProviderError::ApiError("400 bad request".into()).is_transient());
        assert!(!ProviderError::InvalidResponse("garbage".into()).is_transient());
        assert!(!ProviderError::TokenLimitExceeded { used: 9, max: 8 }.is_transient());
        assert!(!ProviderError::Other("mystery".into()).is_transient());
        // An unusable provider is permanent: retrying just burns the job
        // timeout against an account that has no money in it.
        assert!(
            !ProviderError::Unavailable {
                reason: UnavailableReason::CreditsExhausted,
                detail: "HTTP 402".into(),
            }
            .is_transient()
        );
    }

    // ─── Retry advice ───────────────────────────────────────────────────────

    #[test]
    fn a_capacity_refusal_is_told_apart_from_an_ordinary_server_failure() {
        // The distinction the slow backoff hangs on: a 429 or a 529 describes a
        // window that lasts minutes, a 500 or a reset connection a blip.
        for err in [
            ProviderError::RateLimitExceeded {
                retry_after_secs: None,
            },
            ProviderError::ApiError("HTTP 529: {\"type\":\"overloaded_error\"}".into()),
            ProviderError::ApiError("HTTP 429 Too Many Requests".into()),
            ProviderError::ApiError("Overloaded".into()),
            ProviderError::ApiError("rate limit exceeded".into()),
        ] {
            assert!(err.retry_advice().capacity, "{err}");
        }
        for err in [
            ProviderError::ApiError("HTTP 500 Internal Server Error".into()),
            ProviderError::ApiError("HTTP 502 Bad Gateway".into()),
            ProviderError::RequestFailed("connection reset".into()),
            ProviderError::Other("mystery".into()),
            ProviderError::TokenLimitExceeded { used: 9, max: 8 },
        ] {
            assert!(!err.retry_advice().capacity, "{err}");
        }
    }

    #[test]
    fn only_a_rate_limit_carries_the_servers_own_answer() {
        // The header exists on the 429 path and nowhere else, so every other
        // error asks for the caller's own backoff rather than a wait it made up.
        assert_eq!(
            ProviderError::RateLimitExceeded {
                retry_after_secs: Some(42),
            }
            .retry_advice()
            .retry_after_secs,
            Some(42)
        );
        assert_eq!(
            ProviderError::ApiError("HTTP 529 overloaded".into())
                .retry_advice()
                .retry_after_secs,
            None
        );
        assert_eq!(
            ProviderError::RequestFailed("reset".into())
                .retry_advice()
                .retry_after_secs,
            None
        );
    }

    #[test]
    fn no_advice_at_all_is_the_ordinary_schedule() {
        // The default is what a permanent error and a plain blip both answer,
        // and it must not accidentally read as "at capacity".
        let advice = RetryAdvice::default();
        assert!(!advice.capacity);
        assert_eq!(advice.retry_after_secs, None);
    }

    // ─── Provider-fatal classification ──────────────────────────────────────

    /// A labelled message still classifies. The `[kind]` prefix goes in front
    /// of the status, and a prefix that hid it would have silently stopped a 402
    /// from failing over - which is the behaviour the status extraction exists
    /// to drive, and the bug the prefix introduced before this.
    #[test]
    fn a_labelled_message_still_yields_its_status() {
        assert_eq!(
            UnavailableReason::from_message("[server-error] HTTP 402: out of credits"),
            Some(UnavailableReason::CreditsExhausted)
        );
        assert_eq!(
            UnavailableReason::from_message("HTTP 401: bad key"),
            Some(UnavailableReason::AuthFailed),
            "an unlabelled message is unaffected"
        );
        assert_eq!(
            UnavailableReason::from_message("[not-a-kind HTTP 402: unterminated"),
            None,
            "an unclosed bracket is not a prefix to step over"
        );
    }

    /// A resolver that answers every name with a lookup failure.
    ///
    /// Asking the real resolver for a name under `.invalid` is a network round
    /// trip, and on a macOS runner whose resolver is slow the two-second
    /// request deadline fires first: the error comes back as `Timeout` and the
    /// test fails on nothing to do with the code under test. What the test
    /// needs is the error `reqwest` builds when a lookup fails, and the
    /// resolver hook produces exactly that one, on the same wrapper the system
    /// resolver's failure travels in.
    struct NeverResolves;

    impl reqwest::dns::Resolve for NeverResolves {
        fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
            Box::pin(async move {
                let message = format!("failed to lookup address information: {}", name.as_str());
                Err(std::io::Error::other(message).into())
            })
        }
    }

    /// Against the real client stack, because this is the whole point: untold
    /// apart these arrive as one string, and `Display` on a `reqwest::Error`
    /// says the same sentence for both of them.
    ///
    /// Which kind a dead port produces is the OS's business and not the same
    /// everywhere - a Windows runner drops the SYN and the request times out
    /// where a Unix box answers with a refusal - so what is asserted here is the
    /// part that is true everywhere: the two are told apart, and neither is
    /// mistaken for the other. Exactly which kind each chain maps to is settled
    /// by the `connect_failure` tests, which do not go near a socket.
    #[tokio::test]
    async fn a_name_that_does_not_resolve_is_told_from_a_port_with_nothing_behind_it() {
        let unresolvable = http::outbound_builder(Some(2))
            .dns_resolver(NeverResolves)
            .build()
            .expect("a client builds");
        let dns = unresolvable
            .get("https://no-such-host-anywhere-12345.invalid/v1/models")
            .send()
            .await
            .expect_err("does not resolve");
        assert_eq!(FailureKind::from_reqwest(&dns), FailureKind::DnsFailure);

        let client = build_http_client(Some(2)).expect("a client builds");

        // A port nothing is listening on: an ephemeral one is claimed and the
        // listener dropped straight away, which is a port no other process holds.
        // Port 1 was used here, on the reasoning that binding it needs privileges
        // - true, and not the same thing as nothing being behind it.
        let closed = {
            let claimed = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
            claimed.local_addr().expect("has an address")
        };
        let dead_port = client
            .get(format!("http://{closed}/v1/models"))
            .send()
            .await
            .expect_err("nothing answers");
        let dead = FailureKind::from_reqwest(&dead_port);
        assert_ne!(
            dead,
            FailureKind::DnsFailure,
            "a port with nothing behind it is not a name that did not resolve"
        );
        // Membership rather than `matches!`, and the message read out first: an
        // arm no platform takes and an argument only a failure evaluates are
        // both uncovered regions on an assertion that has to pass.
        let seen = format!("{dead:?}");
        let plausible = [FailureKind::ConnectionRefused, FailureKind::Timeout].contains(&dead);
        assert!(
            plausible,
            "a dead port is refused where the OS refuses and timed out where it \
             drops the SYN, and nothing else: {seen}"
        );
    }

    /// The statuses that are not already an `UnavailableReason`. 404 has its own
    /// kind because it reads as "the provider is broken" when it is usually a
    /// base URL pointing at the wrong path.
    #[test]
    fn a_status_is_placed_by_whose_fault_it_is() {
        assert_eq!(FailureKind::from_status(400), FailureKind::BadRequest);
        assert_eq!(FailureKind::from_status(404), FailureKind::NotFound);
        assert_eq!(FailureKind::from_status(500), FailureKind::ServerError);
        assert_eq!(FailureKind::from_status(503), FailureKind::ServerError);
        assert_eq!(FailureKind::from_status(418), FailureKind::BadRequest);
    }

    /// Every kind has a label and a remedy, because both are published: the
    /// label to logs and the API, the remedy to whoever has to fix it.
    #[test]
    fn every_kind_is_labelled_and_has_a_remedy() {
        for kind in [
            FailureKind::DnsFailure,
            FailureKind::ConnectionRefused,
            FailureKind::TlsFailure,
            FailureKind::Timeout,
            FailureKind::ConnectionDropped,
            FailureKind::Transport,
            FailureKind::BadRequest,
            FailureKind::NotFound,
            FailureKind::ServerError,
            FailureKind::MalformedResponse,
        ] {
            // Read out first: an argument evaluated only on failure is its own
            // uncovered region on an assertion that has to pass.
            let label = kind.label();
            assert!(!label.is_empty());
            assert!(!kind.remedy().is_empty(), "{label}");
            assert!(
                !label.contains(' '),
                "a label is one token for a metrics attribute: {label}"
            );
        }
    }

    /// The kind survives the round trip through the message, which is the only
    /// channel that reaches a Rhai script and comes back.
    ///
    /// Driven by a server that accepts and never answers, because that is the
    /// one real failure every platform agrees on: a dead port is refused on some
    /// and timed out on others, and this test is about the round trip rather
    /// than about which kind went into it.
    #[tokio::test]
    async fn the_kind_is_readable_back_off_the_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
        let addr = listener.local_addr().expect("has an address");
        std::thread::spawn(move || {
            let _held = listener.accept();
            std::thread::sleep(std::time::Duration::from_secs(30));
        });

        let client = build_http_client(Some(1)).expect("a client builds");
        let e = client
            .get(format!("http://{addr}/v1/models"))
            .send()
            .await
            .expect_err("never answers");

        let err = ProviderError::transport("sending the request", &e);
        assert_eq!(err.failure_kind(), Some(FailureKind::Timeout));

        let text = err.to_string();
        assert!(text.contains("sending the request"), "{text}");
        assert!(text.contains("reachable but slow or wedged"), "{text}");
    }

    /// An error carrying no label answers `None` rather than guessing one.
    #[test]
    fn an_unlabelled_error_names_no_kind() {
        assert_eq!(
            ProviderError::RequestFailed("no label here".to_string()).failure_kind(),
            None
        );
        assert_eq!(
            ProviderError::ApiError("[not-a-kind] something".to_string()).failure_kind(),
            None
        );
        // A parse failure is a kind without needing a label: the variant is the
        // statement.
        assert_eq!(
            ProviderError::InvalidResponse("bad json".to_string()).failure_kind(),
            Some(FailureKind::MalformedResponse)
        );
        assert_eq!(
            ProviderError::RateLimitExceeded {
                retry_after_secs: None
            }
            .failure_kind(),
            None
        );
    }

    #[test]
    fn classify_maps_the_provider_fatal_statuses() {
        assert_eq!(
            UnavailableReason::classify(402, ""),
            Some(UnavailableReason::CreditsExhausted)
        );
        assert_eq!(
            UnavailableReason::classify(401, ""),
            Some(UnavailableReason::AuthFailed)
        );
        assert_eq!(
            UnavailableReason::classify(403, ""),
            Some(UnavailableReason::Forbidden)
        );
    }

    #[test]
    fn classify_reads_the_body_when_the_status_alone_is_innocent() {
        // Anthropic reports a drained balance as a 400. Without the body scan
        // this reads as a malformed request and kills the run instead of
        // failing over.
        assert_eq!(
            UnavailableReason::classify(
                400,
                r#"{"error":{"message":"Your credit balance is too low to access the API"}}"#
            ),
            Some(UnavailableReason::CreditsExhausted)
        );
        for body in [
            "insufficient credits",
            "This request requires more credits, or fewer max_tokens",
            r#"{"error":{"code":"insufficient_quota"}}"#,
            "You exceeded your current quota",
            "please check your billing details",
        ] {
            assert_eq!(
                UnavailableReason::classify(400, body),
                Some(UnavailableReason::CreditsExhausted),
                "{body}"
            );
        }
    }

    #[test]
    fn from_message_reads_the_status_back_out_of_the_text() {
        // The Rhai path hands over one formatted string, so the status has to
        // come from the message or a script provider never fails over.
        assert_eq!(
            UnavailableReason::from_message("HTTP 402 Payment Required: {}"),
            Some(UnavailableReason::CreditsExhausted)
        );
        assert_eq!(
            UnavailableReason::from_message("HTTP 401: bad key"),
            Some(UnavailableReason::AuthFailed)
        );
        assert_eq!(
            UnavailableReason::from_message("HTTP 403: not allowed"),
            Some(UnavailableReason::Forbidden)
        );
        // No prefix, but the body still says what happened.
        assert_eq!(
            UnavailableReason::from_message("your credit balance is too low"),
            Some(UnavailableReason::CreditsExhausted)
        );
        assert_eq!(UnavailableReason::from_message("HTTP 400: bad field"), None);
        assert_eq!(UnavailableReason::from_message("something broke"), None);
    }

    #[test]
    fn a_number_in_the_body_is_not_mistaken_for_a_status() {
        // "402" appearing in a token count must not read as Payment Required;
        // only the `HTTP ` prefix every provider formats counts.
        assert_eq!(leading_http_status("you requested 402 tokens"), None);
        assert_eq!(leading_http_status("HTTP 402 Payment Required"), Some(402));
        assert_eq!(leading_http_status("HTTP notanumber"), None);
        assert_eq!(leading_http_status(""), None);
        assert_eq!(
            UnavailableReason::from_message("you requested 402 tokens"),
            None
        );
    }

    #[test]
    fn classify_leaves_an_ordinary_failure_alone() {
        // A plain bad request says nothing about the provider's usability, so
        // it must not trip failover or the circuit breaker.
        assert_eq!(
            UnavailableReason::classify(400, "unknown field `foo`"),
            None
        );
        assert_eq!(UnavailableReason::classify(404, "no such model"), None);
        assert_eq!(UnavailableReason::classify(500, "boom"), None);
    }

    #[test]
    fn unavailable_reason_is_reported_only_for_unavailable() {
        let err = ProviderError::Unavailable {
            reason: UnavailableReason::AuthFailed,
            detail: "HTTP 401: bad key".into(),
        };
        assert_eq!(
            err.unavailable_reason(),
            Some(UnavailableReason::AuthFailed)
        );
        assert_eq!(
            ProviderError::ApiError("HTTP 400".into()).unavailable_reason(),
            None
        );
    }

    #[test]
    fn a_transport_failure_counts_as_an_unreachable_provider() {
        // A refused connection says nothing about the request and everything
        // about the provider, and the retry policy has already spent four
        // attempts on it. Leaving it as an ordinary error is what made an
        // OpenRouter-only install die at iteration 0: `ollama` registers with
        // no key, every bundled blueprint lists it, and a dead localhost:11434
        // killed the run instead of falling over to the model behind it.
        assert_eq!(
            ProviderError::RequestFailed("error sending request".into()).unavailable_reason(),
            Some(UnavailableReason::Unreachable)
        );
        // Still worth retrying first - the two questions are separate.
        assert!(ProviderError::RequestFailed("error sending request".into()).is_transient());
        // Everything that is genuinely about the request stays put.
        for err in [
            ProviderError::InvalidResponse("garbage".into()),
            ProviderError::TokenLimitExceeded { used: 9, max: 8 },
            ProviderError::RateLimitExceeded {
                retry_after_secs: Some(5),
            },
            ProviderError::Other("mystery".into()),
        ] {
            assert_eq!(err.unavailable_reason(), None, "{err}");
        }
    }

    #[test]
    fn unavailable_display_leads_with_the_remedy_and_keeps_the_detail() {
        // A raw JSON blob is not a run status. The message must say what to
        // do; the blob stays available.
        let err = ProviderError::Unavailable {
            reason: UnavailableReason::CreditsExhausted,
            detail: "HTTP 402 Payment Required: {\"error\":{}}".into(),
        };
        let msg = err.to_string();
        assert!(msg.starts_with("out of credits:"), "{msg}");
        assert!(msg.contains("top up the account"), "{msg}");
        assert!(msg.contains("HTTP 402 Payment Required"), "{msg}");
    }

    #[test]
    fn every_unavailable_reason_has_a_label_and_a_remedy() {
        for reason in [
            UnavailableReason::CreditsExhausted,
            UnavailableReason::AuthFailed,
            UnavailableReason::Forbidden,
            UnavailableReason::Unreachable,
        ] {
            assert!(!reason.label().is_empty());
            assert!(!reason.remedy().is_empty());
            // The label is a metrics attribute and a `lev ps` cell, so it must
            // stay a single lowercase token.
            assert_eq!(reason.label(), reason.label().to_ascii_lowercase());
            assert!(!reason.label().contains(' '));
        }
    }

    #[test]
    fn unavailable_reason_round_trips_through_serde() {
        // It rides on `DaemonHealth`, which crosses the control socket.
        let json = serde_json::to_string(&UnavailableReason::CreditsExhausted)
            .expect("UnavailableReason serializes");
        assert_eq!(json, "\"credits_exhausted\"");
        let back: UnavailableReason =
            serde_json::from_str(&json).expect("UnavailableReason deserializes");
        assert_eq!(back, UnavailableReason::CreditsExhausted);
    }

    // ─── ProviderError Display ──────────────────────────────────────────────

    #[test]
    fn provider_error_request_failed_display() {
        let err = ProviderError::RequestFailed("timeout".into());
        assert_eq!(err.to_string(), "Request failed: timeout");
    }

    #[test]
    fn provider_error_api_error_display() {
        let err = ProviderError::ApiError("bad request".into());
        assert_eq!(err.to_string(), "API error: bad request");
    }

    #[test]
    fn provider_error_rate_limit_display() {
        let err = ProviderError::RateLimitExceeded {
            retry_after_secs: Some(30),
        };
        // The hint is for the retry loop, not for the reader: the message stays
        // the one sentence a user needs.
        assert_eq!(err.to_string(), "Rate limit exceeded");
    }

    #[test]
    fn provider_error_invalid_response_display() {
        let err = ProviderError::InvalidResponse("missing field".into());
        assert_eq!(err.to_string(), "Invalid response: missing field");
    }

    #[test]
    fn provider_error_token_limit_display() {
        let err = ProviderError::TokenLimitExceeded {
            used: 500,
            max: 100,
        };
        assert_eq!(err.to_string(), "Token limit exceeded: 500 > 100");
    }

    #[test]
    fn provider_error_other_display() {
        let err = ProviderError::Other("something went wrong".into());
        assert_eq!(err.to_string(), "something went wrong");
    }

    // ─── ModelCapabilities default ──────────────────────────────────────────

    #[test]
    fn model_capabilities_default() {
        let caps = ModelCapabilities::default();
        assert!(caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert!(caps.supports_tools);
        assert!(caps.supports_system_prompt);
        assert_eq!(caps.max_context_tokens, 8192);
        assert_eq!(caps.max_output_tokens, 4096);
    }

    // ─── parse_openai_finish_reason ─────────────────────────────────────────

    #[test]
    fn parse_finish_reason_stop() {
        assert_eq!(parse_openai_finish_reason("stop"), FinishReason::Complete);
    }

    #[test]
    fn parse_finish_reason_tool_calls() {
        assert_eq!(
            parse_openai_finish_reason("tool_calls"),
            FinishReason::ToolCall
        );
    }

    #[test]
    fn parse_finish_reason_length() {
        assert_eq!(
            parse_openai_finish_reason("length"),
            FinishReason::TokenLimit
        );
    }

    /// Empty text is a call with no arguments; JSON is JSON; anything else
    /// is kept as the text it was, for the runtime to report as cut off.
    #[test]
    fn tool_arguments_parse_or_are_kept_as_text() {
        assert_eq!(parse_tool_arguments("  "), serde_json::json!({}));
        assert_eq!(
            parse_tool_arguments("{\"a\": 1}"),
            serde_json::json!({"a": 1})
        );
        assert_eq!(
            parse_tool_arguments("{\"path\": \"re"),
            serde_json::json!("{\"path\": \"re")
        );
    }

    #[test]
    fn parse_finish_reason_unknown_is_kept_apart_from_complete() {
        assert_eq!(parse_openai_finish_reason("unknown"), FinishReason::Unknown);
    }

    // ─── Serialization round-trips ──────────────────────────────────────────

    #[test]
    fn token_usage_serde_roundtrip() {
        let usage = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cached_tokens: 20,
            cache_write_tokens: 10,
            reported_cost_usd: None,
        };
        let json = serde_json::to_string(&usage).unwrap();
        let back: TokenUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.prompt_tokens, 100);
        assert_eq!(back.completion_tokens, 50);
        assert_eq!(back.total_tokens, 150);
        assert_eq!(back.cached_tokens, 20);
        assert_eq!(back.cache_write_tokens, 10);
    }

    #[test]
    fn token_usage_cached_defaults_to_zero() {
        let json = r#"{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}"#;
        let usage: TokenUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.cached_tokens, 0);
        assert_eq!(usage.cache_write_tokens, 0);
    }

    #[test]
    fn message_cache_breakpoint_skipped_when_false() {
        let msg = Message {
            role: "user".into(),
            content: "hello".into(),
            cache_breakpoint: false,
            reasoning: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert!(json.get("cache_breakpoint").is_none());
    }

    #[test]
    fn message_cache_breakpoint_included_when_true() {
        let msg = Message {
            role: "system".into(),
            content: "you are helpful".into(),
            cache_breakpoint: true,
            reasoning: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["cache_breakpoint"], true);
    }

    #[test]
    fn inference_request_serde_roundtrip() {
        let req = InferenceRequest {
            system: vec![],
            messages: vec![Message {
                role: "user".into(),
                content: "hi".into(),
                cache_breakpoint: false,
                reasoning: None,
            }],
            model: "gpt-4".into(),
            max_tokens: 100,
            temperature: 0.7,
            tools: vec![Tool {
                name: "search".into(),
                description: "Search the web".into(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: InferenceRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.model, "gpt-4");
        assert_eq!(back.messages.len(), 1);
        assert_eq!(back.tools.len(), 1);
        assert_eq!(back.tools[0].name, "search");
    }

    #[test]
    fn tool_call_serde_roundtrip() {
        let tc = ToolCall {
            id: "call_123".into(),
            name: "get_weather".into(),
            arguments: serde_json::json!({"city": "NYC"}),
            thought_signature: None,
        };
        let json = serde_json::to_string(&tc).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "call_123");
        assert_eq!(back.name, "get_weather");
        assert_eq!(back.arguments["city"], "NYC");
    }

    #[test]
    fn finish_reason_serde_roundtrip() {
        for reason in [
            FinishReason::Complete,
            FinishReason::TokenLimit,
            FinishReason::ToolCall,
            FinishReason::Stop,
        ] {
            let json = serde_json::to_string(&reason).unwrap();
            let back: FinishReason = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{:?}", reason), format!("{:?}", back));
        }
    }

    #[test]
    fn rate_limit_config_serde() {
        let cfg = RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: RateLimitConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.requests_per_minute, 60);
        assert_eq!(back.tokens_per_minute, 100_000);
    }

    // ─── stream_once ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn stream_once_yields_single_item() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll, Waker};

        let mut stream = stream_once::once(42);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);

        assert_eq!(
            Pin::new(&mut stream).poll_next(&mut cx),
            Poll::Ready(Some(42))
        );
        assert_eq!(Pin::new(&mut stream).poll_next(&mut cx), Poll::Ready(None));
    }

    // ─── Default trait method impls (infer_stream, list_models) ────────────

    struct MinimalProvider;

    #[async_trait]
    impl Provider for MinimalProvider {
        async fn infer(&self, _request: &InferenceRequest) -> Result<InferenceResponse> {
            Ok(InferenceResponse {
                content: "hello".to_string(),
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "search".to_string(),
                    arguments: serde_json::json!({"q": "rust"}),
                    thought_signature: None,
                }],
                tokens_used: TokenUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                    reported_cost_usd: None,
                },
                finish_reason: FinishReason::Complete,
                reasoning: None,
            })
        }

        async fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len()
        }

        fn max_context_tokens(&self, _model: &str) -> usize {
            1000
        }

        fn name(&self) -> &str {
            "minimal"
        }

        fn capabilities(&self, model: &str) -> ModelCapabilities {
            // Knows one model and nothing else, so `serves_model` has a real
            // answer to give in both directions. Everything else falls through
            // to the fallback capabilities, which is how a provider says it does
            // not recognise a model.
            let mut caps = ModelCapabilities::default();
            if model == "only-this-one" {
                caps.max_context_tokens += 1;
            }
            caps
        }
    }

    /// The question a blueprint's bare model name asks of every provider. A
    /// provider answers from its own table, so the one model it names comes back
    /// with the id to call it by and everything else comes back as "not mine".
    #[test]
    fn serves_model_answers_from_the_capability_table() {
        assert_eq!(
            MinimalProvider.serves_model("only-this-one"),
            Some("only-this-one".to_string()),
            "a model the table names is served, under the id it was asked about"
        );
        assert_eq!(
            MinimalProvider.serves_model("some-other-model"),
            None,
            "a model the table does not name is not claimed"
        );
    }

    /// A provider whose capability table is compiled in has nothing to fetch,
    /// and must not have to say so.
    #[tokio::test]
    async fn default_prime_capabilities_does_nothing_and_succeeds() {
        MinimalProvider
            .prime_capabilities()
            .await
            .expect("a provider that needs no priming reports success");
    }

    #[tokio::test]
    async fn default_infer_stream_yields_single_chunk_from_infer() {
        use tokio_stream::StreamExt;

        let provider = MinimalProvider;
        let request = InferenceRequest {
            system: vec![],
            messages: vec![],
            model: "any".to_string(),
            max_tokens: 10,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        let mut stream = provider.infer_stream(&request).await.unwrap();
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "hello");
        assert_eq!(chunk.tool_calls.len(), 1);
        assert_eq!(chunk.tool_calls[0].index, 0);
        assert_eq!(chunk.tool_calls[0].id.as_deref(), Some("call_1"));
        assert_eq!(chunk.tool_calls[0].name.as_deref(), Some("search"));
        assert_eq!(chunk.tokens.as_ref().unwrap().total_tokens, 2);
        assert_eq!(chunk.finish_reason, Some(FinishReason::Complete));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn default_list_models_returns_empty() {
        let provider = MinimalProvider;
        let models = provider.list_models().await.unwrap();
        assert!(models.is_empty());
    }

    /// A provider that has not implemented `served_catalog` says "cannot say",
    /// never "serves nothing". The difference decides whether a caller may
    /// refuse a model, and the default has to be the one that refuses nothing -
    /// an empty `Some(vec![])` here would make every model named against every
    /// unimplemented provider wrong.
    ///
    /// Contrast `serves_model` just above, which answers `None` for both "not
    /// mine" and "cannot say". Separating the two is the whole point of this
    /// method existing.
    #[test]
    fn default_served_catalog_says_nothing_rather_than_nothing_served() {
        assert_eq!(MinimalProvider.served_catalog(), None);
    }

    /// A provider that has not implemented `pricing` reports no rates, and that
    /// is the safe default: an unpriced call makes its run report UNKNOWN
    /// rather than contributing zero to a total that then reads as authoritative.
    #[test]
    fn default_pricing_is_unknown_rather_than_free() {
        assert_eq!(MinimalProvider.pricing("any-model"), None);
    }

    #[tokio::test]
    async fn minimal_provider_trait_accessors() {
        let provider = MinimalProvider;
        assert_eq!(provider.count_tokens("hello", "any").await, 5);
        assert_eq!(provider.max_context_tokens("any"), 1000);
        assert_eq!(provider.name(), "minimal");
        assert_eq!(
            provider.capabilities("any").max_context_tokens,
            ModelCapabilities::default().max_context_tokens
        );
    }

    // ─── check_http_response ────────────────────────────────────────────────

    async fn spawn_mock_response(
        status: u16,
        reason: &str,
        headers: &[(&str, &str)],
        body: &'static [u8],
    ) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut header_lines = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            status,
            reason,
            body.len()
        );
        for (k, v) in headers {
            header_lines.push_str(&format!("{}: {}\r\n", k, v));
        }
        header_lines.push_str("\r\n");
        let response_bytes = header_lines.into_bytes();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(&response_bytes).await;
            let _ = socket.write_all(body).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        });
        reqwest::get(format!("http://{}", addr)).await.unwrap()
    }

    async fn spawn_truncated_error_response(status: u16, reason: &str) -> reqwest::Response {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let header = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: 9999\r\nConnection: close\r\n\r\nshort",
            status, reason
        )
        .into_bytes();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(&header).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        });
        reqwest::get(format!("http://{}", addr)).await.unwrap()
    }

    #[tokio::test]
    async fn check_http_response_success_returns_response() {
        let response = spawn_mock_response(200, "OK", &[], b"ok").await;
        let result = check_http_response(response, None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn check_http_response_non_success_returns_api_error() {
        let response = spawn_mock_response(500, "Internal Server Error", &[], b"boom").await;
        let err = check_http_response(response, None).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("500"));
        assert!(msg.contains("boom"));
    }

    #[tokio::test]
    async fn check_http_response_402_becomes_unavailable_not_api_error() {
        // The shape that matters: OpenRouter answering 402 with its
        // credit-balance JSON. As an `ApiError` the whole message is the blob,
        // and it kills the run.
        let body = br#"{"error":{"message":"This request requires more credits, or fewer max_tokens. You requested up to 65536 tokens, but can only afford 28."}}"#;
        let response = spawn_mock_response(402, "Payment Required", &[], body).await;
        let err = check_http_response(response, None).await.unwrap_err();
        assert_eq!(
            err.unavailable_reason(),
            Some(UnavailableReason::CreditsExhausted)
        );
        assert!(!err.is_transient());
        let msg = err.to_string();
        assert!(msg.starts_with("out of credits:"), "{msg}");
        assert!(msg.contains("402"), "{msg}");
    }

    #[tokio::test]
    async fn check_http_response_400_with_a_credit_body_is_still_unavailable() {
        // Anthropic's shape: the status is innocent, the body is not.
        let body = br#"{"error":{"message":"Your credit balance is too low to access the Anthropic API"}}"#;
        let response = spawn_mock_response(400, "Bad Request", &[], body).await;
        let err = check_http_response(response, None).await.unwrap_err();
        assert_eq!(
            err.unavailable_reason(),
            Some(UnavailableReason::CreditsExhausted)
        );
    }

    #[tokio::test]
    async fn check_http_response_ordinary_4xx_stays_an_api_error() {
        let response = spawn_mock_response(404, "Not Found", &[], b"no such model").await;
        let err = check_http_response(response, None).await.unwrap_err();
        assert_eq!(err.unavailable_reason(), None);
        assert!(err.to_string().starts_with("API error:"), "{err}");
    }

    fn assert_contains_500(msg: &str) {
        assert!(msg.contains("500"), "expected 500 in: {msg}");
    }

    #[test]
    #[should_panic(expected = "expected 500 in: not the status you're looking for")]
    fn assert_contains_500_panics_when_missing() {
        assert_contains_500("not the status you're looking for");
    }

    #[tokio::test]
    async fn check_http_response_non_success_body_read_error_falls_back_to_error_string() {
        let response = spawn_truncated_error_response(500, "Internal Server Error").await;
        let err = check_http_response(response, None).await.unwrap_err();
        let msg = err.to_string();
        assert_contains_500(&msg);
    }

    #[tokio::test]
    async fn check_http_response_rate_limited_without_limiter_returns_rate_limit_exceeded() {
        let response = spawn_mock_response(429, "Too Many Requests", &[], b"slow down").await;
        let err = check_http_response(response, None).await.unwrap_err();
        // A 429 with no header is still a capacity refusal, with no hint to
        // honor - the retry loop falls back to its own capacity backoff.
        assert_eq!(
            err.retry_advice(),
            RetryAdvice {
                capacity: true,
                retry_after_secs: None,
            }
        );
    }

    #[tokio::test]
    async fn check_http_response_rate_limited_with_retry_after_notifies_limiter() {
        use crate::rate_limit::RateLimiter;
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        });
        let response = spawn_mock_response(
            429,
            "Too Many Requests",
            &[("retry-after", "2")],
            b"slow down",
        )
        .await;
        let err = check_http_response(response, Some(&limiter))
            .await
            .unwrap_err();
        // The header reaches the limiter *and* the error: the limiter paces the
        // next request, the error tells the retry loop how long to wait before
        // repeating this one.
        assert_eq!(
            err.retry_advice(),
            RetryAdvice {
                capacity: true,
                retry_after_secs: Some(2),
            }
        );
    }

    #[tokio::test]
    async fn check_http_response_rate_limited_with_non_numeric_retry_after_is_ignored() {
        use crate::rate_limit::RateLimiter;
        let limiter = RateLimiter::new(&RateLimitConfig {
            requests_per_minute: 60,
            tokens_per_minute: 100_000,
        });
        let response = spawn_mock_response(
            429,
            "Too Many Requests",
            &[("retry-after", "not-a-number")],
            b"slow down",
        )
        .await;
        let err = check_http_response(response, Some(&limiter))
            .await
            .unwrap_err();
        // An HTTP-date or any other unreadable value is no hint at all, which
        // is the same answer as a missing header rather than a failure.
        assert_eq!(
            err.retry_advice(),
            RetryAdvice {
                capacity: true,
                retry_after_secs: None,
            }
        );
    }

    // ─── build_http_client ─────────────────────────────────────────────────

    #[test]
    fn build_http_client_with_timeout() {
        let client = build_http_client(Some(30)).expect("an HTTPS client builds in tests");
        // Should successfully build a client; we cannot inspect the timeout
        // directly, but confirming it doesn't panic is the coverage goal.
        drop(client);
    }

    #[test]
    fn build_http_client_without_timeout() {
        let client = build_http_client(None).expect("an HTTPS client builds in tests");
        drop(client);
    }

    /// Regression for the read_files hang: a connection where the server
    /// accepts the request but never sends a response must ERROR, not block
    /// forever. This is the exact shape of the hang the user hit - a large
    /// request accepted by Anthropic (h2 WindowUpdate seen) with no response
    /// ever returned. The bound is now the per-request timeout applied by
    /// [`apply_request_timeout`] (on top of the fresh-connection fix), so this
    /// exercises the production `build_http_client` + `apply_request_timeout`
    /// path with a short 2s deadline; production defaults to
    /// `DEFAULT_INFERENCE_TIMEOUT_SECS`.
    #[tokio::test]
    async fn per_request_timeout_aborts_a_connection_that_never_responds() {
        use std::time::{Duration, Instant};
        use tokio::io::AsyncReadExt;

        // Server: accept one connection, drain the request, then hold the
        // socket open forever without writing any response.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept succeeds");
            let mut buf = [0u8; 4096];
            // Keep reading (draining the request) and never respond. This is a
            // diverging `loop` (type `!`) with no `break`, so there is no
            // data-dependent branch and no unreachable task-exit tail; the
            // runtime aborts the task at test end.
            loop {
                let _ = sock.read(&mut buf).await;
            }
        });

        // The production client (no client-level duration cap) plus a short
        // per-request deadline. If the per-request timeout were not applied this
        // send would hang and the test would time out instead of asserting.
        let client = build_http_client(None).expect("an HTTPS client builds in tests");
        let start = Instant::now();
        let builder = client
            .post(format!("http://{addr}/"))
            .body("request body that never gets a response");
        let result = apply_request_timeout(builder, Some(2)).send().await;
        let elapsed = start.elapsed();

        assert!(
            result.is_err(),
            "a silent server must yield an error, not a response"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "per-request timeout should abort at ~2s; took {elapsed:?} (it did not fire)"
        );
    }

    #[test]
    fn apply_request_timeout_none_is_a_noop() {
        // The `None` arm must return the builder unchanged (no per-request cap);
        // build a request both ways and confirm both are constructible.
        let client = build_http_client(None).expect("an HTTPS client builds in tests");
        let with_none = apply_request_timeout(client.post("https://example.invalid/"), None);
        let with_some = apply_request_timeout(client.post("https://example.invalid/"), Some(5));
        assert!(with_none.build().is_ok());
        assert!(with_some.build().is_ok());
    }

    // ─── MessageContent::as_text for Blocks variant ────────────────────────

    #[test]
    fn message_content_as_text_blocks_mixed() {
        let content = MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "hello ".to_string(),
            },
            ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "search".to_string(),
                input: serde_json::json!({}),
                thought_signature: None,
            },
            ContentBlock::Text {
                text: "world".to_string(),
            },
            ContentBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: "result".to_string(),
                is_error: false,
            },
        ]);
        // Only Text blocks are concatenated; ToolUse and ToolResult are skipped.
        assert_eq!(content.as_text(), "hello world");
    }

    // ─── MessageContent::as_text for Text variant ──────────────────────────

    #[test]
    fn message_content_as_text_plain_string() {
        let content = MessageContent::Text("just words".to_string());
        assert_eq!(content.as_text(), "just words");
    }

    // ─── MessageContent::from &str ─────────────────────────────────────────

    #[test]
    fn message_content_from_str_ref() {
        let content: MessageContent = "hi there".into();
        assert!(matches!(&content, MessageContent::Text(s) if s == "hi there"));
    }

    // ─── FinishReason equality ─────────────────────────────────────────────

    #[test]
    fn finish_reason_stop_eq_stop() {
        assert_eq!(FinishReason::Stop, FinishReason::Stop);
    }

    #[test]
    fn finish_reason_different_variants_not_eq() {
        assert_ne!(FinishReason::Stop, FinishReason::Complete);
        assert_ne!(FinishReason::TokenLimit, FinishReason::ToolCall);
    }

    // ─── decode_json: transport failure vs schema failure ──────────────────

    /// Serve one HTTP response over a raw socket, then close it.
    ///
    /// Takes the literal bytes so a test can send a deliberately malformed or
    /// truncated response, which the higher-level test servers go out of their
    /// way to make impossible.
    async fn serve_raw(raw: &'static [u8]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept succeeds");
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(raw).await;
            let _ = sock.shutdown().await;
        });
        format!("http://{addr}/")
    }

    /// A body that stops early is a *transport* failure, not a parse failure.
    ///
    /// This is what the whole split exists for: a socket that dies mid-body (a
    /// reset, or a machine that slept through the response) surfacing as
    /// `InvalidResponse` - which `is_transient` calls permanent - kills a run
    /// with dozens of iterations of work behind it without one retry.
    #[tokio::test]
    async fn decode_json_reports_a_truncated_body_as_a_transport_failure() {
        // Declares 200 bytes, sends 9, then hangs up.
        let url = serve_raw(b"HTTP/1.1 200 OK\r\nContent-Length: 200\r\n\r\n{\"a\": 1}\n").await;
        let client = build_http_client(None).expect("an HTTPS client builds in tests");
        let response = client.get(url).send().await.expect("headers arrive");

        let err = decode_json::<serde_json::Value>(response)
            .await
            .expect_err("a truncated body cannot decode");

        assert_eq!(
            std::mem::discriminant(&err),
            std::mem::discriminant(&ProviderError::RequestFailed(String::new())),
            "a body that never finished arriving is a transport failure: {err}"
        );
        assert!(err.is_transient(), "it must be retried: {err}");
        assert_eq!(
            err.unavailable_reason(),
            Some(UnavailableReason::Unreachable),
            "it must count against the provider and allow failover: {err}"
        );
    }

    /// Bytes that all arrived and did not fit the schema stay permanent: the
    /// same request would produce the same unusable answer, so retrying it only
    /// spends the attempts.
    #[tokio::test]
    async fn decode_json_reports_a_complete_but_unparseable_body_as_invalid() {
        let url = serve_raw(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nnot json").await;
        let client = build_http_client(None).expect("an HTTPS client builds in tests");
        let response = client.get(url).send().await.expect("headers arrive");

        let err = decode_json::<serde_json::Value>(response)
            .await
            .expect_err("`not json` cannot parse");

        assert_eq!(
            std::mem::discriminant(&err),
            std::mem::discriminant(&ProviderError::InvalidResponse(String::new())),
            "a fully delivered body that does not parse is the provider's fault: {err}"
        );
        assert!(!err.is_transient(), "retrying cannot help: {err}");
        assert_eq!(
            err.unavailable_reason(),
            None,
            "the provider is fine: {err}"
        );
    }

    /// A body that keeps coming past the cap stops the read where the cap
    /// is, and the error says which cap and which peer.
    #[tokio::test]
    async fn decode_json_refuses_a_body_past_the_cap() {
        let mut raw = b"HTTP/1.1 200 OK\r\nContent-Length: 20000\r\n\r\n".to_vec();
        raw.extend(std::iter::repeat_n(b'[', 20000));
        let url = serve_raw(raw.leak()).await;
        let client = build_http_client(None).expect("an HTTPS client builds in tests");
        let response = client.get(url).send().await.expect("headers arrive");

        let err = decode_json_capped::<serde_json::Value>(response, 4096)
            .await
            .expect_err("a body past the cap cannot decode");

        assert_eq!(
            err.to_string(),
            "Invalid response: response body exceeded 4096 bytes from 127.0.0.1"
        );
        assert!(
            !err.is_transient(),
            "the same request draws the same body: {err}"
        );
    }

    /// The happy path, so the success arm is not carried by the failure tests.
    #[tokio::test]
    async fn decode_json_parses_a_well_formed_body() {
        let url = serve_raw(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\n{\"ok\":true}\n").await;
        let client = build_http_client(None).expect("an HTTPS client builds in tests");
        let response = client.get(url).send().await.expect("headers arrive");

        let body: serde_json::Value = decode_json(response).await.expect("it parses");
        assert_eq!(body["ok"], serde_json::json!(true));
    }
}
