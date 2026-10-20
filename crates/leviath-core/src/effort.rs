//! How hard a stage asks its model to think, in words every provider maps.
//!
//! Each provider spells its reasoning control differently: `reasoning.effort`
//! on the Responses routes and OpenRouter, `output_config.effort` on Claude,
//! `thinking_level` on Gemini, `reasoningConfig` on Nova. A stage names one
//! [`Effort`], and the provider that ends up serving each request turns it
//! into its own field, or into nothing when the model has no such control.

use serde::{Deserialize, Serialize};

/// Effort levels from least to most, as the Responses APIs, OpenRouter and
/// Claude spell them. `none` is left out: it turns reasoning off, which a
/// model that must reason refuses, and no [`Effort`] asks for it.
pub const LEVELS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

/// How much reasoning a stage asks of its model.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    /// As little as the model takes.
    Low,
    /// A moderate amount.
    Medium,
    /// A thorough pass.
    High,
}

impl Effort {
    /// The level as providers spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// The level `text` names, if it names one.
    pub fn parse(text: &str) -> Option<Self> {
        [Self::Low, Self::Medium, Self::High]
            .into_iter()
            .find(|e| e.as_str() == text)
    }

    /// The level to send a model whose accepted levels are `accepted`.
    ///
    /// This level when the model takes it, or when nothing is listed. Else
    /// the least listed level above it, so a model that takes only `high`
    /// still answers. Else the most listed level below it. `None` when the
    /// list is empty: the model takes no effort at all.
    pub fn clamp(self, accepted: Option<&[String]>) -> Option<&'static str> {
        let takes = |level: &str| accepted.is_none_or(|a| a.iter().any(|l| l == level));
        let at = LEVELS.iter().position(|l| *l == self.as_str()).unwrap_or(0);
        let (below, above) = LEVELS.split_at(at);
        above
            .iter()
            .copied()
            .find(|l| takes(l))
            .or_else(|| below.iter().rev().copied().find(|l| takes(l)))
    }
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn levels(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn each_level_reads_back_as_written() {
        for effort in [Effort::Low, Effort::Medium, Effort::High] {
            assert_eq!(Effort::parse(effort.as_str()), Some(effort));
            assert_eq!(effort.to_string(), effort.as_str());
            let json = serde_json::to_value(effort).unwrap();
            assert_eq!(json, serde_json::json!(effort.as_str()));
        }
        assert_eq!(Effort::parse("none"), None);
        assert!(Effort::Low < Effort::High);
    }

    #[test]
    fn a_level_the_model_takes_is_sent_as_it_is() {
        assert_eq!(Effort::Medium.clamp(None), Some("medium"));
        let all = levels(&["low", "medium", "high"]);
        assert_eq!(Effort::Low.clamp(Some(&all)), Some("low"));
    }

    #[test]
    fn a_missing_level_moves_up_first_then_down() {
        let only_high = levels(&["high"]);
        assert_eq!(Effort::Low.clamp(Some(&only_high)), Some("high"));
        let low_and_high = levels(&["low", "high"]);
        assert_eq!(Effort::Medium.clamp(Some(&low_and_high)), Some("high"));
        let only_minimal = levels(&["none", "minimal"]);
        assert_eq!(Effort::High.clamp(Some(&only_minimal)), Some("minimal"));
    }

    #[test]
    fn a_model_that_takes_no_level_is_sent_none() {
        assert_eq!(Effort::Low.clamp(Some(&[])), None);
    }
}
