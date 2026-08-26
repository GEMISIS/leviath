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
pub mod capabilities;
pub mod claude_code;
#[cfg(feature = "debug-http")]
pub mod debug_http;
pub mod failure;
pub mod gemini;
pub mod ollama;
pub mod openai;
pub mod openai_compat;
pub mod openrouter;
pub mod pricing;
pub mod provider;
pub mod rate_limit;
pub mod rhai_provider;
pub mod text_tools;
pub mod tokenizer;

#[cfg(test)]
mod test_support;

pub use anthropic::AnthropicProvider;
pub use capabilities::{LimitsSource, ModelCapabilities, ModelCapabilityOverride};
pub use claude_code::ClaudeCodeProvider;
pub use gemini::GeminiProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenAIProvider;
pub use openrouter::OpenRouterProvider;
pub use pricing::{CostTotals, ModelPricing};
pub use provider::{
    ContentBlock, DEFAULT_INFERENCE_TIMEOUT_SECS, FailureKind, FinishReason, InferenceRequest,
    InferenceResponse, Message, MessageContent, ModelInfo, Provider, ProviderConfig, ProviderError,
    RateLimitConfig, Result, RetryAdvice, SystemBlock, TokenUsage, Tool, ToolCall,
    UnavailableReason, build_http_client, collect_stream,
};
pub use rhai_provider::RhaiProvider;

#[cfg(test)]
mod gateway_tests;
