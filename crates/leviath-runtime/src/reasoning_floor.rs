//! The least reasoning a model will take, for a call that wants none.
//!
//! The title call asks for no reasoning at all, and on OpenRouter that is
//! `reasoning: { enabled: false }`. A model that always reasons refuses it:
//! Muse Spark answers with a 400 saying reasoning "is mandatory for this
//! endpoint and cannot be disabled". Such a model is asked for the lowest
//! effort it accepts instead.
//!
//! Three things say a model must reason, checked in this order:
//! - a refusal seen earlier in this process, remembered per provider and model;
//! - the gateway's own catalogue, whose `reasoning.mandatory` says so outright
//!   and whose `supported_efforts` names the levels it takes;
//! - for a model the catalogue says nothing about, a short list of families
//!   that always reason.
//!
//! Any call that turns reasoning off goes through [`floor_reasoning`], so a
//! second caller cannot drift from the title call's rule.

use leviath_providers::{InferenceRequest, Provider, ProviderError};

/// Model id prefixes whose models always reason, for when the catalogue has
/// not been read or does not describe the model. Meta's Muse models reason on
/// every request, directly and through OpenRouter.
const ALWAYS_REASONS: &[&str] = &["meta/"];

/// Effort levels from least to most, as OpenRouter and the Responses APIs
/// spell them. `none` is left out: it is the switch a model that must reason
/// refuses.
const EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

/// Provider and model pairs that refused to turn reasoning off in this
/// process, so later calls to them ask for the least effort from the start.
static MUST_REASON: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<(String, String)>>,
> = std::sync::LazyLock::new(Default::default);

/// Remember that `provider` refused to turn reasoning off for `model`.
pub(crate) fn remember_must_reason(provider: &str, model: &str) {
    leviath_core::sync::lock(&MUST_REASON).insert((provider.to_string(), model.to_string()));
}

/// Whether `provider` refused to turn reasoning off for `model` earlier in
/// this process.
fn remembered(provider: &str, model: &str) -> bool {
    leviath_core::sync::lock(&MUST_REASON).contains(&(provider.to_string(), model.to_string()))
}

/// Whether a failed call was the provider refusing to turn reasoning off: a
/// 400 whose body carries the refusal's own phrase, "reasoning is
/// mandatory". The two words apart are not enough: a 400 about a mandatory
/// field of the reasoning object is a different mistake, and taking it for
/// this one would remember a model as one that must reason.
pub(crate) fn refuses_reasoning_off(error: &ProviderError) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    let bad_request = error.failure_kind() == Some(leviath_providers::FailureKind::BadRequest)
        || text.contains("http 400");
    bad_request && text.contains("reasoning is mandatory")
}

/// What the provider's catalogue says about `model`, under the id it lists
/// it by, when the catalogue was read and names it.
pub(crate) fn listed_model(
    provider: &dyn Provider,
    model: &str,
) -> Option<(String, leviath_providers::LearnedModel)> {
    let learned = provider.learned_models()?;
    let id = learned.find_by_key(model)?;
    learned.get(&id).map(|entry| (id, entry))
}

/// The effort to ask a model for when it must reason, or `None` when it may
/// be told not to.
///
/// `low` when the catalogue lists it or lists nothing, which matches what the
/// title call asks of every Responses route. A model whose levels leave `low`
/// out gets the least of the ones it names: `openai/gpt-5-pro` takes only
/// `high`.
fn least_effort(provider: &dyn Provider, model: &str) -> Option<&'static str> {
    let listed = listed_model(provider, model);
    let id = listed.as_ref().map_or(model, |(id, _)| id.as_str());
    let mandatory = listed
        .as_ref()
        .and_then(|(_, entry)| entry.reasoning_mandatory);
    let family = ALWAYS_REASONS.iter().any(|p| id.starts_with(p));
    let must = remembered(provider.name(), model) || mandatory.unwrap_or(family);
    let efforts = listed.and_then(|(_, entry)| entry.reasoning_efforts);
    must.then(|| pick_effort(efforts.as_deref()))
}

/// `low` unless `efforts` names levels without it, then the least it names.
fn pick_effort(efforts: Option<&[String]>) -> &'static str {
    let takes = |level: &str| efforts.is_none_or(|e| e.iter().any(|l| l == level));
    match takes("low") {
        true => "low",
        false => EFFORTS.iter().copied().find(|l| takes(l)).unwrap_or("low"),
    }
}

/// Whether `extra` turns reasoning off with OpenRouter's switch.
fn turns_reasoning_off(extra: &serde_json::Value) -> bool {
    extra.pointer("/reasoning/enabled") == Some(&serde_json::Value::Bool(false))
}

/// Replace a "no reasoning" switch in `extra` with the least effort `model`
/// accepts, when it must reason. Anything else is left as it is.
pub(crate) fn floor_reasoning(provider: &dyn Provider, model: &str, extra: &mut serde_json::Value) {
    if !turns_reasoning_off(extra) {
        return;
    }
    if let Some(effort) = least_effort(provider, model) {
        extra["reasoning"] = serde_json::json!({ "effort": effort });
    }
}

/// Whether a call that failed with `error` should go again with the least
/// reasoning: the provider refused a request that turned reasoning off. The
/// refusal is remembered, and the request comes back with its switch floored;
/// `None` for any other failure. Once per call, because the request sent
/// again no longer turns reasoning off.
pub(crate) fn after_refusal(
    provider: &dyn Provider,
    request: &InferenceRequest,
    error: &ProviderError,
) -> Option<InferenceRequest> {
    if !turns_reasoning_off(&request.extra) || !refuses_reasoning_off(error) {
        return None;
    }
    remember_must_reason(provider.name(), &request.model);
    let mut extra = request.extra.clone();
    floor_reasoning(provider, &request.model, &mut extra);
    Some(InferenceRequest {
        extra,
        ..request.clone()
    })
}

#[cfg(test)]
#[path = "reasoning_floor_tests.rs"]
mod tests;
