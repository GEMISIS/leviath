//! Error types for Leviath Core.

use thiserror::Error;

/// Result type alias using Leviath's Error type.
pub type Result<T> = std::result::Result<T, Error>;

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
}
