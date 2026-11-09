//! Turning [`UserSettings`] into the defaults a stage resolves against and
//! the data retention settings a registry carries.

use super::settings::UserSettings;
use crate::pipeline::ModelDefaults;
use crate::spec::names::ModelRef;
use leviath_providers::retention::{Retention, RetentionSettings, request_knobs};
use std::collections::HashMap;

/// The defaults a stage resolves against: the default provider, the override
/// and fallback models, the provider order, the fallback chain, and the
/// retention settings.
///
/// A malformed `[providers] fallback_order` entry is dropped and named in
/// `warnings` rather than failing: a typo in a safety net should not stop a
/// host from starting. Splitting on the first `/` keeps model ids that
/// contain one (`deepseek/deepseek-v4-flash`) intact.
pub fn model_defaults(settings: &UserSettings, warnings: &mut Vec<String>) -> ModelDefaults {
    let fallback_order = settings
        .providers
        .fallback_order
        .iter()
        .filter_map(|raw| match ModelRef::parse(raw) {
            Ok(route) if route.provider.is_some() => Some(route),
            _ => {
                warnings.push(format!(
                    "ignoring [providers] fallback_order entry {raw:?}: expected \"provider/model\""
                ));
                None
            }
        })
        .collect();
    ModelDefaults {
        provider: settings.default_provider.clone(),
        override_model: settings.override_model.clone(),
        fallback_model: settings.fallback_model.clone(),
        fallback_order,
        provider_order: settings.providers.provider_order.clone(),
        retention: retention_settings(settings, warnings),
    }
}

/// The data retention settings the file carries, in the registry's form: the
/// request for zero retention, the declared agreements, and the `retention`
/// words on `[model_capabilities]` and `[model_providers]` entries.
///
/// A `[model_providers]` entry spells its words as any other key, since a
/// script entry forwards every key to its script, so one that is not a
/// retention is named in `warnings` and ignored rather than failing the
/// load. Every entry is read, scripts included, in name order so the
/// warnings come out the same way each time.
pub fn retention_settings(
    settings: &UserSettings,
    warnings: &mut Vec<String>,
) -> RetentionSettings {
    let mut entries: Vec<_> = settings.model_providers.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));

    let mut provider_declarations = HashMap::new();
    let mut request_knob_aliases = HashMap::new();
    for (name, entry) in entries {
        if let Some(word) = entry.extra.get("retention").and_then(toml::Value::as_str) {
            match Retention::parse(word) {
                Ok(retention) => {
                    provider_declarations.insert(name.clone(), retention);
                }
                Err(e) => {
                    warnings.push(format!("ignoring [model_providers.{name}] retention: {e}"));
                }
            }
        }
        // Which built-in's per-request fields an endpoint takes. Only names
        // the table has fields for mean anything; another is a typo.
        if let Some(target) = entry
            .extra
            .get("zero_retention_request")
            .and_then(toml::Value::as_str)
        {
            if request_knobs(target).is_some() {
                request_knob_aliases.insert(name.clone(), target.to_string());
            } else {
                warnings.push(format!(
                    "ignoring [model_providers.{name}] zero_retention_request = \"{target}\": no \
                     built-in provider of that name takes a per-request field (openai, openrouter)"
                ));
            }
        }
    }
    RetentionSettings {
        zero_requested: settings.providers.zero_retention,
        agreements: settings.providers.zero_retention_agreements.clone(),
        model_overrides: settings
            .model_capabilities
            .iter()
            .filter_map(|(model, caps)| caps.retention.map(|r| (model.clone(), r)))
            .collect(),
        provider_declarations,
        request_knob_aliases,
        file_uploads: settings.providers.file_uploads,
    }
}
