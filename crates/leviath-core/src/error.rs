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

#[cfg(test)]
mod tests {
    use super::*;

    // ─── ValidationError Display ────────────────────────────────────────────

    #[test]
    fn validation_error_blueprint() {
        let e = ValidationError::Blueprint("missing name".into());
        assert_eq!(e.to_string(), "Invalid blueprint: missing name");
    }

    #[test]
    fn validation_error_stage() {
        let e = ValidationError::Stage {
            stage: "init".into(),
            message: "no prompt".into(),
        };
        assert_eq!(e.to_string(), "Invalid stage 'init': no prompt");
    }

    #[test]
    fn validation_error_region() {
        let e = ValidationError::Region {
            region: "context".into(),
            message: "too large".into(),
        };
        assert_eq!(e.to_string(), "Invalid region 'context': too large");
    }

    #[test]
    fn validation_error_layout() {
        let e = ValidationError::Layout("overlapping regions".into());
        assert_eq!(e.to_string(), "Invalid layout: overlapping regions");
    }

    #[test]
    fn validation_error_graph() {
        let e = ValidationError::Graph("cycle detected".into());
        assert_eq!(e.to_string(), "Invalid graph: cycle detected");
    }

    #[test]
    fn validation_error_transition() {
        let e = ValidationError::Transition {
            from: "A".into(),
            to: "B".into(),
            message: "missing condition".into(),
        };
        assert_eq!(
            e.to_string(),
            "Invalid transition from 'A' to 'B': missing condition"
        );
    }

    // ─── Error Display ──────────────────────────────────────────────────────

    #[test]
    fn error_region_not_found() {
        let e = Error::RegionNotFound("history".into());
        assert_eq!(e.to_string(), "Region not found: history");
    }

    #[test]
    fn error_validation_failed() {
        let e = Error::ValidationFailed("bad input".into());
        assert_eq!(e.to_string(), "Region validation failed: bad input");
    }

    #[test]
    fn error_token_budget_exceeded() {
        let e = Error::TokenBudgetExceeded {
            used: 500,
            max: 100,
        };
        assert_eq!(e.to_string(), "Content exceeds token budget: 500 > 100");
    }

    #[test]
    fn error_pinned_regions_over_budget() {
        let e = Error::PinnedRegionsOverBudget {
            pinned_tokens: 2000,
            total_budget: 1000,
        };
        assert_eq!(
            e.to_string(),
            "Pinned regions (2000) exceed total budget (1000)"
        );
    }

    #[test]
    fn error_blueprint_invalid() {
        let e = Error::BlueprintInvalid("parse error".into());
        assert_eq!(e.to_string(), "Blueprint validation failed: parse error");
    }

    #[test]
    fn error_layout_invalid() {
        let e = Error::LayoutInvalid("bad layout".into());
        assert_eq!(e.to_string(), "Layout validation failed: bad layout");
    }

    #[test]
    fn error_transform_failed() {
        let e = Error::TransformFailed("script error".into());
        assert_eq!(e.to_string(), "Context transform failed: script error");
    }

    #[test]
    fn error_other() {
        let e = Error::Other("misc".into());
        assert_eq!(e.to_string(), "misc");
    }

    #[test]
    fn error_from_serde_json() {
        let json_err = serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
        let e = Error::from(json_err);
        assert!(e.to_string().contains("Serialization error"));
    }

    // ─── Clone for ValidationError ──────────────────────────────────────────

    #[test]
    fn validation_error_is_cloneable() {
        let e = ValidationError::Graph("cycle".into());
        let cloned = e.clone();
        assert_eq!(e.to_string(), cloned.to_string());
    }
}
