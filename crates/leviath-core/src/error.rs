//! Error types for Leviath Core.

use thiserror::Error;

/// Result type alias using Leviath's Error type.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors from blueprint, stage, region, and layout validation.
#[derive(Debug, Clone, Error)]
pub enum ValidationError {
    /// Blueprint-level validation failure
    #[error("Invalid blueprint: {0}")]
    Blueprint(String),

    /// Stage-level validation failure
    #[error("Invalid stage '{stage}': {message}")]
    Stage { stage: String, message: String },

    /// Region-level validation failure
    #[error("Invalid region '{region}': {message}")]
    Region { region: String, message: String },

    /// Layout-level validation failure
    #[error("Invalid layout: {0}")]
    Layout(String),

    /// Graph structure validation failure
    #[error("Invalid graph: {0}")]
    Graph(String),

    /// Transition validation failure
    #[error("Invalid transition from '{from}' to '{to}': {message}")]
    Transition {
        from: String,
        to: String,
        message: String,
    },
}

/// Core error types for Leviath.
#[derive(Error, Debug)]
pub enum Error {
    /// Region with the specified name was not found
    #[error("Region not found: {0}")]
    RegionNotFound(String),

    /// Region validation failed
    #[error("Region validation failed: {0}")]
    ValidationFailed(String),

    /// Content exceeds region's token budget
    #[error("Content exceeds token budget: {used} > {max}")]
    TokenBudgetExceeded { used: usize, max: usize },

    /// Pinned regions alone exceed total token budget
    #[error("Pinned regions ({pinned_tokens}) exceed total budget ({total_budget})")]
    PinnedRegionsOverBudget {
        pinned_tokens: usize,
        total_budget: usize,
    },

    /// Blueprint validation failed
    #[error("Blueprint validation failed: {0}")]
    BlueprintInvalid(String),

    /// Layout validation failed
    #[error("Layout validation failed: {0}")]
    LayoutInvalid(String),

    /// Context transform failed
    #[error("Context transform failed: {0}")]
    TransformFailed(String),

    /// Serialization error
    #[error("Serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),

    /// Generic error
    #[error("{0}")]
    Other(String),
}
