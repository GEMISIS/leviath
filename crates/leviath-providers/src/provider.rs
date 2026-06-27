//! Provider trait and common types.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Result type for provider operations.
pub type Result<T> = std::result::Result<T, ProviderError>;

/// Errors that can occur during provider operations.
#[derive(Error, Debug)]
pub enum ProviderError {
    /// HTTP request failed
    #[error("Request failed: {0}")]
    RequestFailed(String),

    /// API returned an error
    #[error("API error: {0}")]
    ApiError(String),

    /// Rate limit exceeded
    #[error("Rate limit exceeded")]
    RateLimitExceeded,

    /// Invalid response from provider
    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    /// Token limit exceeded
    #[error("Token limit exceeded: {used} > {max}")]
    TokenLimitExceeded { used: usize, max: usize },

    /// Other error
    #[error("{0}")]
    Other(String),
}

/// Request for LLM inference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceRequest {
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
}

/// A message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Role (system, user, assistant)
    pub role: String,

    /// Message content
    pub content: String,
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
}

/// Token usage breakdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Tokens in the prompt
    pub prompt_tokens: usize,

    /// Tokens in the completion
    pub completion_tokens: usize,

    /// Total tokens
    pub total_tokens: usize,
}

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
}

/// Trait for LLM providers.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Execute inference with the given request.
    async fn infer(&self, request: InferenceRequest) -> Result<InferenceResponse>;

    /// Count tokens in the given text for this provider's models.
    fn count_tokens(&self, text: &str, model: &str) -> usize;

    /// Get the maximum context tokens for a model.
    fn max_context_tokens(&self, model: &str) -> usize;

    /// Get the provider name.
    fn name(&self) -> &str;
}
