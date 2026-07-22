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
pub mod claude_code;
#[cfg(feature = "debug-http")]
pub mod debug_http;
pub mod gemini;
pub mod ollama;
pub mod openai;
pub mod openai_compat;
pub mod openrouter;
pub mod provider;
pub mod rate_limit;
pub mod text_tools;
pub mod tokenizer;

#[cfg(test)]
mod test_support;

pub use anthropic::AnthropicProvider;
pub use claude_code::ClaudeCodeProvider;
pub use gemini::GeminiProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenAIProvider;
pub use openrouter::OpenRouterProvider;
pub use provider::{
    ContentBlock, FinishReason, InferenceRequest, InferenceResponse, Message, MessageContent,
    ModelCapabilities, ModelInfo, Provider, ProviderConfig, ProviderError, RateLimitConfig, Result,
    StreamChunk, SystemBlock, TokenUsage, Tool, ToolCall, ToolCallDelta, build_http_client,
    check_http_response, parse_openai_finish_reason,
};
pub use rate_limit::RateLimiter;
