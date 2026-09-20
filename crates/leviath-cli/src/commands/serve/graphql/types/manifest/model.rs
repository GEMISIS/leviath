//! Which model a stage runs on, and what it asks of it.

use async_graphql::{SimpleObject, Union};

use super::count;
use crate::commands::serve::graphql::scalars::Json;

/// One provider and model, in a stage's ordered list.
#[derive(Debug, SimpleObject)]
pub(crate) struct StageModelRoute {
    /// The provider that serves it, as the config names providers.
    pub(crate) provider: String,
    /// The model id, as that provider spells it.
    pub(crate) model: String,
}

/// A fixed number of output tokens, sent as written.
#[derive(Debug, SimpleObject)]
pub(crate) struct MaxTokensCount {
    /// The number of tokens.
    pub(crate) tokens: i32,
}

/// A share of the model's context window.
///
/// What a stage that rewrites a whole document wants: the number that fits is
/// the model's, not the author's, and it changes with the model the stage lands
/// on. The resolved value is clamped to the model's own maximum.
#[derive(Debug, SimpleObject)]
pub(crate) struct MaxTokensContextPercent {
    /// The share, as a percentage.
    pub(crate) percent: f64,
}

/// A share of one region's token budget.
///
/// What a stage that fills a region wants: a reply larger than the region it
/// goes into is cut somewhere, and the region's budget is the honest ceiling.
#[derive(Debug, SimpleObject)]
pub(crate) struct MaxTokensRegionPercent {
    /// The share, as a percentage.
    pub(crate) percent: f64,
    /// The region whose budget it is a share of, by name.
    pub(crate) region: String,
}

/// How large one reply may be.
///
/// Three shapes because a fixed number is the wrong answer to two of the three
/// questions. A relative cap resolves against the model or the region at
/// inference time, so a stage moved to a larger model uses it.
#[derive(Debug, Union)]
pub(crate) enum MaxOutputTokens {
    /// A number of tokens.
    Count(MaxTokensCount),
    /// A share of the model's window.
    ContextPercent(MaxTokensContextPercent),
    /// A share of a region's budget.
    RegionPercent(MaxTokensRegionPercent),
}

impl From<leviath_core::blueprint::OutputCap> for MaxOutputTokens {
    fn from(cap: leviath_core::blueprint::OutputCap) -> Self {
        use leviath_core::blueprint::OutputCap as Core;
        match cap {
            Core::Tokens(tokens) => Self::Count(MaxTokensCount {
                tokens: count(tokens),
            }),
            Core::WindowPercent(fraction) => Self::ContextPercent(MaxTokensContextPercent {
                percent: fraction * 100.0,
            }),
            Core::RegionPercent { percent, region } => {
                Self::RegionPercent(MaxTokensRegionPercent {
                    percent: percent * 100.0,
                    region,
                })
            }
        }
    }
}

/// What a stage asks of whichever model it lands on.
///
/// `temperature` and `maxOutputTokens` are pulled out because every provider
/// takes them and a client renders them. Everything else stays in
/// `providerParams` as written: a parameter one provider understands is not a
/// parameter we should invent a field for, and dropping it would lose it.
#[derive(Debug, SimpleObject)]
pub(crate) struct ModelParameters {
    /// How much the model may wander, when the stage sets it.
    pub(crate) temperature: Option<f64>,
    /// The largest reply this stage will accept.
    pub(crate) max_output_tokens: Option<MaxOutputTokens>,
    /// Every other parameter, as the manifest wrote it.
    pub(crate) provider_params: Json,
}

impl ModelParameters {
    /// Read a stage's parameter table.
    ///
    /// A `max_output_tokens` that does not parse is left out rather than
    /// guessed at. The manifest loader refuses such a blueprint, so reaching
    /// that here means the file changed underneath an installed run, and
    /// reporting a cap nobody wrote would be worse than reporting none.
    pub(crate) fn from_table(
        parameters: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Self {
        let temperature = parameters.get("temperature").and_then(|v| v.as_f64());
        let max_output_tokens = parameters
            .get("max_output_tokens")
            .and_then(|value| leviath_core::blueprint::OutputCap::parse(value).ok())
            .map(MaxOutputTokens::from);
        let rest: serde_json::Map<String, serde_json::Value> = parameters
            .iter()
            .filter(|(key, _)| key.as_str() != "temperature" && key.as_str() != "max_output_tokens")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Self {
            temperature,
            max_output_tokens,
            provider_params: Json(serde_json::Value::Object(rest)),
        }
    }
}

/// A stage's model block.
#[derive(Debug, SimpleObject)]
pub(crate) struct StageModelConfig {
    /// The models to try, best first. The first whose provider is configured on
    /// this machine is the one that runs.
    pub(crate) models: Vec<StageModelRoute>,
    /// Whether the machine's own default model may stand in when none of the
    /// listed models is configured. False makes the list a requirement.
    pub(crate) allow_user_default: bool,
    /// What the stage asks of the model it lands on.
    pub(crate) parameters: ModelParameters,
    /// A per-stage deadline for one inference, in seconds, including retries.
    /// Null leaves the daemon's own deadline in place.
    pub(crate) request_timeout_secs: Option<i32>,
}

impl From<&leviath_core::blueprint::ModelConfig> for StageModelConfig {
    fn from(model: &leviath_core::blueprint::ModelConfig) -> Self {
        Self {
            models: model
                .models
                .iter()
                .map(|entry| StageModelRoute {
                    provider: entry.provider.clone(),
                    model: entry.model.clone(),
                })
                .collect(),
            allow_user_default: model.allow_user_default,
            parameters: ModelParameters::from_table(&model.parameters),
            request_timeout_secs: model
                .request_timeout_secs
                .map(|secs| i32::try_from(secs).unwrap_or(i32::MAX)),
        }
    }
}
