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

pub use provider::{
    Provider, InferenceRequest, InferenceResponse, Message, Tool, ToolCall,
    TokenUsage, FinishReason, ProviderError, Result, StreamChunk, ToolCallDelta,
    ProviderConfig, RateLimitConfig, ModelCapabilities, ModelInfo,
    parse_openai_finish_reason, check_http_response,
};
pub use rate_limit::RateLimiter;
pub use anthropic::AnthropicProvider;
pub use claude_code::ClaudeCodeProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenAIProvider;
pub use openrouter::OpenRouterProvider;
