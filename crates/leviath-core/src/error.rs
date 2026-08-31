//! Error types for Leviath Core.

use thiserror::Error;

/// Result type alias using Leviath's Error type.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors from blueprint, stage, region, and layout validation.
#[derive(Debug, Clone, Error, PartialEq)]
pub enum ValidationError {
    /// Blueprint-level validation failure
    #[error("Invalid blueprint: {0}")]
    Blueprint(String),

    /// Stage-level validation failure
    #[error("Invalid stage '{stage}': {message}")]
    Stage {
        /// The offending stage's name.
        stage: String,
        /// What is wrong with it.
        message: String,
    },

    /// Region-level validation failure
    #[error("Invalid region '{region}': {message}")]
    Region {
        /// The offending region's name.
        region: String,
        /// What is wrong with it.
        message: String,
    },

    /// Layout-level validation failure
    #[error("Invalid layout: {0}")]
    Layout(String),

    /// Graph structure validation failure
    #[error("Invalid graph: {0}")]
    Graph(String),

    /// Transition validation failure
    #[error("Invalid transition from '{from}' to '{to}': {message}")]
    Transition {
        /// The stage the edge leaves.
        from: String,
        /// The stage the edge names as its target, which may not exist.
        to: String,
        /// What is wrong with it.
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
    TokenBudgetExceeded {
        /// Tokens the write would have brought the region to.
        used: usize,
        /// The region's ceiling.
        max: usize,
    },

    /// A region under `admission = "reject"` is full, and the write was
    /// refused rather than something else being dropped to fit it.
    ///
    /// Distinct from [`Error::TokenBudgetExceeded`] because the remedy is
    /// different: that one says this single write is too big for the region,
    /// this one says the region is full and the agent has to decide what it is
    /// finished with.
    #[error(
        "Region '{region}' is full ({used}/{max} tokens) and does not evict automatically - \
         release an entry before adding another"
    )]
    RegionFull {
        /// The region that refused the write.
        region: String,
        /// Tokens the region currently holds.
        used: usize,
        /// The region's ceiling.
        max: usize,
    },

    /// A custom region's `on_write` hook rejected the write.
    ///
    /// Only raised for agent-origin writes (`context_write`, `context_append`,
    /// routed tool results), where the refusal and its reason can be reported
    /// back to the writer. A framework write that a hook rejects is stored
    /// unchanged with a warning instead - a script must not be able to delete
    /// an assistant turn or a system record.
    #[error("Region '{region}' refused the write: {reason}")]
    RegionRefusedWrite {
        /// The region whose hook refused the write.
        region: String,
        /// Why, as the hook said it (or a generic phrase when it only
        /// returned `false`).
        reason: String,
    },

    /// Pinned regions alone exceed total token budget
    #[error("Pinned regions ({pinned_tokens}) exceed total budget ({total_budget})")]
    PinnedRegionsOverBudget {
        /// Tokens held by regions that can never be evicted, which is what makes
        /// this unrecoverable rather than a matter of dropping something.
        pinned_tokens: usize,
        /// The whole window's budget.
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
    fn error_region_refused_write() {
        let e = Error::RegionRefusedWrite {
            region: "claims".into(),
            reason: "needs a source line".into(),
        };
        assert_eq!(
            e.to_string(),
            "Region 'claims' refused the write: needs a source line"
        );
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
