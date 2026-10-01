//! Which model a stage runs on, and what it asks of it.

use async_graphql::{SimpleObject, Union};
use leviath_graphql_derive::mirror;

use super::count;
use crate::commands::serve::graphql::scalars::Json;

/// One provider and model, in a stage's ordered list.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct StageModelRoute {
    /// The provider that serves it, as the config names providers. Empty when
    /// the blueprint names the model alone and the machine's provider order
    /// picks one.
    pub(crate) provider: String,
    /// The model id, as that provider spells it.
    pub(crate) model: String,
}

/// A fixed number of output tokens, sent as written.
#[mirror]
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
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct MaxTokensContextPercent {
    /// The share, as a percentage.
    pub(crate) percent: f64,
}

/// A share of one region's token budget.
///
/// What a stage that fills a region wants: a reply larger than the region it
/// goes into is cut somewhere, and the region's budget is the honest ceiling.
#[mirror]
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
#[mirror]
#[derive(Debug, Union)]
pub(crate) enum MaxOutputTokens {
    /// A number of tokens.
    Count(MaxTokensCount),
    /// A share of the model's window.
    ContextPercent(MaxTokensContextPercent),
    /// A share of a region's budget.
    RegionPercent(MaxTokensRegionPercent),
}

impl From<&leviath_runtime::spec::graph::OutputCap> for MaxOutputTokens {
    fn from(cap: &leviath_runtime::spec::graph::OutputCap) -> Self {
        use leviath_runtime::spec::graph::OutputCap as Core;
        match cap {
            Core::Tokens(tokens) => Self::Count(MaxTokensCount {
                tokens: count(*tokens),
            }),
            Core::WindowPercent(fraction) => Self::ContextPercent(MaxTokensContextPercent {
                percent: fraction * 100.0,
            }),
            Core::RegionPercent { percent, region } => {
                Self::RegionPercent(MaxTokensRegionPercent {
                    percent: percent * 100.0,
                    region: region.to_string(),
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
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct ModelParameters {
    /// How much the model may wander, when the stage sets it.
    pub(crate) temperature: Option<f64>,
    /// The largest reply this stage will accept.
    pub(crate) max_output_tokens: Option<MaxOutputTokens>,
    /// Every other parameter, as the manifest wrote it.
    pub(crate) provider_params: Json,
}

impl From<&leviath_runtime::spec::graph::ModelParams> for ModelParameters {
    fn from(parameters: &leviath_runtime::spec::graph::ModelParams) -> Self {
        let rest: serde_json::Map<String, serde_json::Value> = parameters
            .extra
            .iter()
            .map(|(key, value)| (key.clone(), value.to_json()))
            .collect();
        Self {
            // Through its shortest decimal form, so the `0.2` the blueprint
            // wrote is served as `0.2` rather than as the nearest `f32` widened.
            temperature: parameters.temperature.map(|t| {
                t.to_string()
                    .parse::<f64>()
                    .expect("an f32 prints as a number")
            }),
            max_output_tokens: parameters
                .max_output_tokens
                .as_ref()
                .map(MaxOutputTokens::from),
            provider_params: Json(serde_json::Value::Object(rest)),
        }
    }
}

impl ModelParameters {
    /// Read the parameters a request really carried, as the inference record
    /// keeps them: `temperature`, `max_output_tokens` in any form a blueprint
    /// may write it, and every other key as written.
    ///
    /// A `max_output_tokens` that does not read as a cap is left out rather
    /// than guessed at: reporting a cap nobody set would be worse than
    /// reporting none.
    pub(crate) fn from_table(
        parameters: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Self {
        let temperature = parameters.get("temperature").and_then(|v| v.as_f64());
        let max_output_tokens = parameters
            .get("max_output_tokens")
            .and_then(|value| {
                serde_json::from_value::<leviath_runtime::spec::graph::OutputCap>(value.clone())
                    .ok()
            })
            .map(|cap| MaxOutputTokens::from(&cap));
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
#[mirror]
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

impl From<&leviath_runtime::spec::graph::ModelChoice> for StageModelConfig {
    fn from(model: &leviath_runtime::spec::graph::ModelChoice) -> Self {
        Self {
            models: model
                .models
                .iter()
                .map(|entry| StageModelRoute {
                    provider: entry
                        .provider
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                    model: entry.model.to_string(),
                })
                .collect(),
            allow_user_default: model.allow_user_default,
            parameters: ModelParameters::from(&model.params),
            request_timeout_secs: model
                .request_timeout_secs
                .map(|secs| i32::try_from(secs).unwrap_or(i32::MAX)),
        }
    }
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
