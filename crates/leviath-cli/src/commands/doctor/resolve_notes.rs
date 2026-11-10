//! The notes the `resolve` check appends: why the first provider in the order
//! lost here,
//! and how a qualified `override_model` or `fallback_model` was read.
//!
//! Split out of `doctor.rs` by concern, so the check itself stays readable
//! beside the wording that explains its result.

use super::*;

/// The note appended when the resolved provider is not the first one in the
/// user's provider order.
///
/// This check resolves an empty `ModelChoice`, so the head of the order
/// really does lose here without an `override_model`: there is no blueprint
/// entry to promote and no model to send. A real run is the opposite case, so
/// the note must not say that provider "is never chosen": that reads as a
/// statement about the reader's runs.
///
/// It is not. `resolve_stage_candidates` moves every registered candidate on
/// a preferred provider to the front of the blueprint's list, so
/// `provider_order = ["openrouter"]` sends every stage of every bundled
/// blueprint to that blueprint's OpenRouter entry. Said the other way, a run
/// quietly executing on a fallback model for weeks would look, from here, like
/// a config line that did nothing at all.
///
/// Not a failure: the resolution is legitimate and the run will work. It is
/// only worth saying because it is not what the config appears to ask for.
/// Silent while `--model` is in play, which is the caller overriding on purpose.
pub(super) fn head_provider_note(
    config: &Config,
    resolved: &str,
    model_override: Option<&str>,
    registry: &ProviderRegistry,
) -> String {
    let Some(named) = config.head_provider().filter(|head| *head != resolved) else {
        return String::new();
    };
    // The missing model is the only reason a registered head of the order
    // loses from here: this check resolves an empty `ModelChoice`, so one with
    // a model set has no competition to lose to. An *unregistered* one is a
    // different complaint, and one the `config` line already makes by listing
    // what is registered.
    if model_override.is_some() || config.override_model.is_some() || !registry.has(named) {
        return String::new();
    }
    format!(
        "  (note: this check resolves no blueprint, so with no `override_model` \
         set there is nothing to send to '{named}' and it loses here. A real run \
         is different: a blueprint that lists '{named}' has that entry moved to \
         the front, so your runs use '{named}' with whatever model the blueprint \
         names for each stage. Set `override_model` only to pin one model across \
         every stage, which overrides the per-stage choices a blueprint makes; \
         `fallback_model` is the setting for a stage that names nothing configured here.)"
    )
}

/// The notes appended when `override_model` or `fallback_model` is written as
/// `provider/model`.
///
/// Both are bare model ids that pair with the first provider in
/// `provider_order`, but `--model`
/// and `fallback_order` take the qualified form and an OpenRouter id already
/// contains a slash, so `override_model = "ollama/qwen3.8:latest"` is an easy
/// thing to write. The resolver drops the redundant prefix, so the run works;
/// this says what it was read as, so the config can be tidied and so the line
/// above is not a mystery. Silent under `--model`, when neither setting is in
/// play at all.
pub(super) fn qualified_user_model_notes(config: &Config, model_override: Option<&str>) -> String {
    if model_override.is_some() {
        return String::new();
    }
    let provider = config.head_provider().unwrap_or_default();
    config
        .qualified_user_models()
        .into_iter()
        .map(|(key, written, bare)| {
            format!(
                "  (note: {key} is written as '{written}', but it takes a bare model id \
                 and pairs with the first provider in provider_order - it is read as \
                 '{bare}'; drop the \
                 '{provider}/' in config.toml)"
            )
        })
        .collect()
}
