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
pub mod ollama;
pub mod openai;
pub mod openrouter;
pub mod provider;
pub mod rate_limit;
pub mod tokenizer;

pub use anthropic::AnthropicProvider;
pub use claude_code::ClaudeCodeProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenAIProvider;
pub use openrouter::OpenRouterProvider;
pub use provider::{
    check_http_response, parse_openai_finish_reason, FinishReason, InferenceRequest,
    InferenceResponse, Message, ModelCapabilities, ModelInfo, Provider, ProviderConfig,
    ProviderError, RateLimitConfig, Result, StreamChunk, TokenUsage, Tool, ToolCall, ToolCallDelta,
};
pub use rate_limit::RateLimiter;
