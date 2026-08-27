//! Which model a stage runs on, and what a user may override.
//!
//! Two levels: a [`ModelEntry`] names a provider and model, and [`ModelConfig`]
//! decides whether the user's own default may stand in for it.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A single model entry within a [`ModelConfig`] models list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelEntry {
    /// Provider name (e.g., "anthropic", "openai")
    pub provider: String,

    /// Model identifier (e.g., "claude-sonnet-4-6")
    pub model: String,
}

impl ModelEntry {
    /// One provider/model pair in a stage's fallback list.
    pub fn new(provider: String, model: String) -> Self {
        Self { provider, model }
    }
}

/// Model configuration for a stage.
///
/// Models are specified as an ordered priority list in `models`. The first
/// entry whose provider is registered at runtime is used. When
/// `allow_user_default` is true (the default), the user's configured default
/// model is tried as a last resort. When false, the stage fails if none of
/// the listed models are available.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Ordered list of models to try (first available wins).
    #[serde(default)]
    pub models: Vec<ModelEntry>,

    /// When true (default), fall back to the user's configured default model
    /// if none of the listed models are available.
    #[serde(default = "default_allow_user_default")]
    pub allow_user_default: bool,

    /// Optional parameters that apply to whichever model gets selected.
    #[serde(default)]
    pub parameters: HashMap<String, serde_json::Value>,

    /// Optional per-stage cap on the wall-clock time (in seconds) one inference
    /// for this stage may run - the whole call including retries. When set, it
    /// overrides the default job timeout; when `None`, the default applies.
    ///
    /// This lets a stage with slow first-token latency (e.g. a large-prompt
    /// analyze call) get a long cap while a quick iterative stage fails fast on
    /// a stalled connection instead of hanging for the full default.
    #[serde(default)]
    pub request_timeout_secs: Option<u64>,
}

fn default_allow_user_default() -> bool {
    true
}

/// How large one reply may be, as a stage's `parameters.max_output_tokens`
/// says it.
///
/// A bare number is the classic form and is sent as written. The other two
/// are relative, because a fixed number is the wrong shape for the question:
/// a stage that rewrites a whole report needs "as much as this model can
/// give", and a stage that fills a region needs "as much as the region
/// holds". A fixed `24000` on the report stage was smaller than the report,
/// and every reply was cut off. A relative cap is clamped to the model's own
/// maximum, since asking for more than that is refused by the provider.
///
/// ```toml
/// parameters = { max_output_tokens = 8000 }              # tokens
/// parameters = { max_output_tokens = "40%" }             # of the model's context window
/// parameters = { max_output_tokens = "100% of claims" }  # of the `claims` region's budget
/// parameters = { max_output_tokens = { percent = 100, of = "claims" } }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum OutputCap {
    /// A fixed number of output tokens, sent as written.
    Tokens(usize),
    /// A fraction (`0.0..=1.0`) of the model's context window.
    WindowPercent(f64),
    /// A fraction (`0.0..=1.0`) of a named region's token budget.
    RegionPercent {
        /// The fraction.
        percent: f64,
        /// The region whose budget it is a fraction of.
        region: String,
    },
}

impl OutputCap {
    /// Read the cap from the parameter's JSON value. `Err` carries the reason
    /// in the words a blueprint author needs, for the manifest loader to
    /// surface: a cap that does not parse must fail the load, because the
    /// alternative (no cap) is exactly the silent no-op that hides a typo.
    pub fn parse(value: &serde_json::Value) -> Result<Self, String> {
        match value {
            serde_json::Value::Number(n) => match n.as_u64() {
                Some(t) if t > 0 => Ok(OutputCap::Tokens(t as usize)),
                _ => Err(format!(
                    "max_output_tokens = {n} must be a positive whole number of tokens"
                )),
            },
            serde_json::Value::String(s) => Self::parse_text(s),
            serde_json::Value::Object(table) => {
                let percent = match table.get("percent") {
                    Some(serde_json::Value::Number(n)) => Self::fraction(&format!("{n}%"))?,
                    Some(serde_json::Value::String(s)) => Self::fraction(s)?,
                    _ => {
                        return Err(
                            "max_output_tokens as a table needs `percent` (a number, or \
                                    a string like \"40%\") and optionally `of = \"<region>\"`"
                                .to_string(),
                        );
                    }
                };
                match table.get("of") {
                    None => Ok(OutputCap::WindowPercent(percent)),
                    Some(serde_json::Value::String(region)) if !region.trim().is_empty() => {
                        Ok(OutputCap::RegionPercent {
                            percent,
                            region: region.trim().to_string(),
                        })
                    }
                    Some(_) => Err("max_output_tokens: `of` must name a region".to_string()),
                }
            }
            other => Err(format!(
                "max_output_tokens = {other} is not a token count, a percentage like \"40%\", \
                 or \"<percent>% of <region>\""
            )),
        }
    }

    /// `"40%"` or `"100% of claims"`.
    fn parse_text(s: &str) -> Result<Self, String> {
        match s.split_once(" of ") {
            None => Ok(OutputCap::WindowPercent(Self::fraction(s)?)),
            Some((pct, region)) => {
                let region = region.trim();
                if region.is_empty() {
                    return Err(format!(
                        "max_output_tokens = \"{s}\" names no region after `of`"
                    ));
                }
                Ok(OutputCap::RegionPercent {
                    percent: Self::fraction(pct)?,
                    region: region.to_string(),
                })
            }
        }
    }

    /// The `(0, 100]` percent rule shared with region budgets, in this
    /// setting's words.
    fn fraction(s: &str) -> Result<f64, String> {
        crate::layout::BudgetSpec::parse_budget(s).map_err(|e| format!("max_output_tokens: {e}"))
    }

    /// The cap in tokens for one request.
    ///
    /// `model_window` and `model_max_output` are the model's own limits;
    /// `region_budget` answers "how many tokens may region X hold" for the
    /// window the request is built from. A relative cap is clamped to the
    /// model's maximum. A region cap naming a region the stage does not carry
    /// falls back to the model's maximum, which is the same "as much as you
    /// can" the author was reaching for; the loader already warned about the
    /// name.
    pub fn resolve(
        &self,
        model_window: usize,
        model_max_output: usize,
        region_budget: impl Fn(&str) -> Option<usize>,
    ) -> usize {
        let share = |whole: usize, fraction: f64| (whole as f64 * fraction).round() as usize;
        match self {
            OutputCap::Tokens(t) => *t,
            OutputCap::WindowPercent(p) => share(model_window, *p).min(model_max_output),
            OutputCap::RegionPercent { percent, region } => match region_budget(region) {
                Some(budget) => share(budget, *percent).min(model_max_output),
                None => model_max_output,
            },
        }
        .max(1)
    }
}

impl ModelConfig {
    /// The stage's output cap, if `parameters.max_output_tokens` sets one.
    pub fn output_cap(&self) -> Result<Option<OutputCap>, String> {
        self.parameters
            .get("max_output_tokens")
            .map(OutputCap::parse)
            .transpose()
    }

    /// Create a new model configuration with a single model entry.
    pub fn new(provider: String, model: String) -> Self {
        Self {
            models: vec![ModelEntry::new(provider, model)],
            allow_user_default: true,
            parameters: HashMap::new(),
            request_timeout_secs: None,
        }
    }

    /// Convenience: provider of the first model entry (for backward compat).
    pub fn provider(&self) -> &str {
        self.models
            .first()
            .map(|e| e.provider.as_str())
            .unwrap_or("anthropic")
    }

    /// Convenience: model name of the first model entry (for backward compat).
    pub fn model(&self) -> &str {
        self.models
            .first()
            .map(|e| e.model.as_str())
            .unwrap_or("claude-sonnet-4-6")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_written_form_of_the_cap_parses() {
        assert_eq!(OutputCap::parse(&json!(8000)), Ok(OutputCap::Tokens(8000)));
        assert_eq!(
            OutputCap::parse(&json!("40%")),
            Ok(OutputCap::WindowPercent(0.4))
        );
        assert_eq!(
            OutputCap::parse(&json!("100% of claims")),
            Ok(OutputCap::RegionPercent {
                percent: 1.0,
                region: "claims".to_string()
            })
        );
        assert_eq!(
            OutputCap::parse(&json!({"percent": 25})),
            Ok(OutputCap::WindowPercent(0.25))
        );
        assert_eq!(
            OutputCap::parse(&json!({"percent": "50%", "of": " report "})),
            Ok(OutputCap::RegionPercent {
                percent: 0.5,
                region: "report".to_string()
            })
        );
    }

    /// Each rejection names what was wrong in the author's terms; none of
    /// them is a silent "no cap".
    #[test]
    fn a_cap_that_does_not_parse_says_why() {
        let err = |v: serde_json::Value| OutputCap::parse(&v).expect_err("rejected");
        assert!(err(json!(0)).contains("positive whole number"));
        assert!(err(json!(-5)).contains("positive whole number"));
        assert!(err(json!("forty")).contains("must end with '%'"));
        assert!(err(json!("150%")).contains("at most 100%"));
        assert!(err(json!("50% of ")).contains("names no region"));
        assert!(err(json!("x% of claims")).contains("not a valid number"));
        assert!(err(json!({"of": "claims"})).contains("needs `percent`"));
        assert!(err(json!({"percent": 0})).contains("greater than 0%"));
        assert!(err(json!({"percent": "abc"})).contains("must end with"));
        assert!(err(json!({"percent": 10, "of": 3})).contains("must name a region"));
        assert!(err(json!({"percent": 10, "of": ""})).contains("must name a region"));
        assert!(err(json!(true)).contains("not a token count"));
    }

    #[test]
    fn a_cap_resolves_against_the_model_and_the_region_and_never_below_one() {
        let budget = |name: &str| (name == "claims").then_some(3_000);
        assert_eq!(
            OutputCap::Tokens(70_000).resolve(200_000, 65_535, budget),
            70_000
        );
        assert_eq!(
            OutputCap::WindowPercent(0.4).resolve(200_000, 65_535, budget),
            65_535
        );
        assert_eq!(
            OutputCap::WindowPercent(0.1).resolve(200_000, 65_535, budget),
            20_000
        );
        let claims = OutputCap::RegionPercent {
            percent: 1.0,
            region: "claims".to_string(),
        };
        assert_eq!(claims.resolve(200_000, 65_535, budget), 3_000);
        assert_eq!(claims.resolve(200_000, 2_000, budget), 2_000);
        let missing = OutputCap::RegionPercent {
            percent: 1.0,
            region: "gone".to_string(),
        };
        assert_eq!(missing.resolve(200_000, 65_535, budget), 65_535);
        assert_eq!(OutputCap::WindowPercent(0.001).resolve(10, 10, budget), 1);
    }

    #[test]
    fn a_model_config_reads_its_own_cap() {
        let mut config = ModelConfig::new("p".to_string(), "m".to_string());
        assert_eq!(config.output_cap(), Ok(None));
        config
            .parameters
            .insert("max_output_tokens".to_string(), json!("30%"));
        assert_eq!(config.output_cap(), Ok(Some(OutputCap::WindowPercent(0.3))));
        config
            .parameters
            .insert("max_output_tokens".to_string(), json!("lots"));
        assert!(config.output_cap().is_err());
    }
}
