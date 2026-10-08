//! # Leviath Providers
//!
//! LLM provider integrations for Leviath.
//!
//! Implements the Provider trait for different LLM providers, handling:
//! - Message construction from context regions
//! - Token counting
//! - Tool calling
//! - Streaming
//! - Rate limiting
//! - Provider-specific features (caching, etc.)

pub mod anthropic;
pub mod bedrock;
pub(crate) mod call_ids;
pub mod capabilities;
pub mod capability_cache;
pub mod codex;
pub mod compiled;
#[cfg(feature = "debug-http")]
pub(crate) mod debug_http;
pub mod endpoint;
pub mod factory;
pub mod failure;
pub mod files;
pub mod gemini;
pub mod grok;
pub mod jobs;
pub mod learned;
#[cfg(any(
    feature = "openai",
    feature = "xai",
    feature = "xai-subscription",
    feature = "google",
    feature = "bedrock",
    feature = "meta",
    feature = "meshy"
))]
pub(crate) mod media;
pub mod meshy;
pub mod meta;
pub mod mime;
pub mod mime_output;
pub mod mime_tables;
pub mod oauth;
pub mod ollama;
pub mod openai;
pub(crate) mod openai_compat;
pub mod openrouter;
pub mod pricing;
pub mod provider;
pub mod quota;
pub mod rate_limit;
pub mod responses;
pub mod retention;
pub mod rhai_provider;
pub(crate) mod text_tools;
pub use text_tools::flatten_tool_turns;
pub mod tokenizer;
pub mod xai;

#[cfg(test)]
mod test_support;

#[cfg(feature = "anthropic")]
pub use anthropic::AnthropicProvider;
#[cfg(feature = "bedrock")]
pub use bedrock::BedrockProvider;
pub use capabilities::{LimitsSource, ModelCapabilities, ModelCapabilityOverride, ModelMime};
pub use capability_cache::{CapabilityCache, CheckOutcome, ProviderCheck};
#[cfg(feature = "openai-subscription")]
pub use codex::CodexProvider;
pub use endpoint::EndpointProvider;
#[cfg(feature = "google")]
pub use gemini::GeminiProvider;
pub use learned::{LearnedModel, LearnedModels};
#[cfg(feature = "meshy")]
pub use meshy::MeshyProvider;
pub use oauth::{ProviderAuthStore, ProviderGrant};
#[cfg(feature = "ollama")]
pub use ollama::OllamaProvider;
#[cfg(feature = "openai")]
pub use openai::OpenAIProvider;
#[cfg(feature = "openrouter")]
pub use openrouter::OpenRouterProvider;
pub use pricing::{CostTotals, ModelPricing};
pub use provider::{
    ContentBlock, DEFAULT_INFERENCE_TIMEOUT_SECS, FailureKind, FinishReason, InferenceRequest,
    InferenceResponse, Message, MessageContent, ModelInfo, OPENING_TURN, Provider, ProviderError,
    RateLimitConfig, Result, RetryAdvice, SystemBlock, TokenUsage, Tool, ToolCall,
    UnavailableReason, build_http_client, collect_stream, tool_input_object,
};
#[cfg(feature = "rhai")]
pub use rhai_provider::RhaiProvider;

#[cfg(all(test, feature = "providers"))]
mod gateway_tests;
