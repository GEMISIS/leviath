//! Turning [`Settings`] into the providers a registry is built from and the
//! defaults a stage resolves against.
//!
//! Mirrors the CLI's `provider_creds_from_config` and `model_defaults` for
//! every provider the narrow settings cover. A test in `leviath-cli` holds the
//! two to the same answer for one config that exercises all of them, so the
//! copy cannot drift without a failing build.

use super::settings::{KEYED, Settings};
use crate::pipeline::ModelDefaults;
use crate::provider_creds::ProviderCreds;
use crate::spec::names::ModelRef;
use leviath_core::CredentialStoreKind;
use std::collections::HashMap;
use std::path::Path;

/// A string with its blank cases taken out: `lev setup` writes empty strings
/// for what the user skipped, and a blank key or host is not one.
fn non_blank(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The `options` spelling of a cache TTL, matching what the config accepts.
fn cache_ttl_key(ttl: leviath_providers::anthropic::CacheTtl) -> &'static str {
    match ttl {
        leviath_providers::anthropic::CacheTtl::Ephemeral5m => "5m",
        leviath_providers::anthropic::CacheTtl::Ephemeral1h => "1h",
    }
}

/// Every provider `settings` opts in, in the order the CLI registers them:
/// the keyed ones with a non-blank key, then Ollama, Codex and Grok.
pub(super) fn provider_creds(settings: &mut Settings, grants_path: &Path) -> Vec<ProviderCreds> {
    let mut creds = Vec::new();
    for keyed in &KEYED {
        let name = keyed.provider();
        let Some(key) = non_blank((keyed.key)(settings).as_deref()) else {
            continue;
        };
        let mut options = HashMap::new();
        if name == "anthropic"
            && let Some(ttl) = settings.providers.anthropic_cache_ttl
        {
            options.insert("cache_ttl".to_string(), cache_ttl_key(ttl).to_string());
        }
        if name == leviath_providers::bedrock::PROVIDER_NAME
            && let Some(region) = non_blank(settings.providers.bedrock_region.as_deref())
        {
            options.insert("region".to_string(), region);
        }
        let base_url = non_blank((keyed.base_url)(settings).as_deref());
        let headers = (keyed.headers)(settings)
            .iter()
            .map(|(header, value)| (header.clone(), value.clone()))
            .collect();
        creds.push(
            ProviderCreds {
                name: name.to_string(),
                api_key: Some(key),
                base_url,
                model_capabilities: settings.model_capabilities.clone(),
                request_timeout_secs: settings.request_timeout_secs,
                rate_limit: settings.rate_limits.get(name).cloned(),
                options,
            }
            .with_headers(headers),
        );
    }

    // Opt-in by the switch or by naming an address, as in the CLI.
    if settings.providers.ollama_enabled || settings.ollama_base_url.is_some() {
        creds.push(ProviderCreds {
            name: "ollama".to_string(),
            api_key: None,
            base_url: Some(
                settings
                    .ollama_base_url
                    .as_deref()
                    .unwrap_or("http://localhost:11434")
                    .to_string(),
            ),
            model_capabilities: settings.model_capabilities.clone(),
            request_timeout_secs: settings.request_timeout_secs,
            rate_limit: settings.rate_limits.get("ollama").cloned(),
            options: HashMap::new(),
        });
    }

    if settings.providers.codex_enabled {
        let name = leviath_providers::codex::PROVIDER_NAME;
        creds.push(ProviderCreds {
            name: name.to_string(),
            api_key: None,
            base_url: None,
            model_capabilities: settings.model_capabilities.clone(),
            request_timeout_secs: settings.request_timeout_secs,
            rate_limit: settings.rate_limits.get(name).cloned(),
            options: codex_options(settings, grants_path),
        });
    }

    if settings.providers.grok_enabled {
        let name = leviath_providers::grok::PROVIDER_NAME;
        creds.push(
            ProviderCreds {
                name: name.to_string(),
                api_key: None,
                base_url: non_blank(settings.providers.xai_base_url.as_deref()),
                model_capabilities: settings.model_capabilities.clone(),
                request_timeout_secs: settings.request_timeout_secs,
                rate_limit: settings.rate_limits.get(name).cloned(),
                options: grant_options(settings, grants_path),
            }
            .with_headers(
                settings
                    .providers
                    .xai_headers
                    .iter()
                    .map(|(header, value)| (header.clone(), value.clone()))
                    .collect(),
            ),
        );
    }

    creds
}

/// What every sign-in provider's options carry: where the grant is, and
/// whether it is in the OS credential store.
fn grant_options(settings: &Settings, grants_path: &Path) -> HashMap<String, String> {
    let mut options = HashMap::new();
    options.insert(
        "auth_store_path".to_string(),
        grants_path.display().to_string(),
    );
    if settings.security.credential_store == CredentialStoreKind::Keychain {
        options.insert("credential_store".to_string(), "keychain".to_string());
    }
    options
}

/// The `options` a Codex [`ProviderCreds`] carries.
fn codex_options(settings: &Settings, grants_path: &Path) -> HashMap<String, String> {
    let mut options = grant_options(settings, grants_path);
    let codex = &settings.providers;
    options.extend(
        codex
            .codex_originator
            .clone()
            .map(|v| ("originator".to_string(), v)),
    );
    options.extend(
        codex
            .codex_reasoning_effort
            .clone()
            .map(|v| ("effort".to_string(), v)),
    );
    options.extend(
        codex
            .codex_verbosity
            .clone()
            .map(|v| ("verbosity".to_string(), v)),
    );
    options.insert(
        "replay_reasoning".to_string(),
        codex.codex_replay_reasoning.to_string(),
    );
    options
}

/// The defaults a stage resolves against, as the CLI's `model_defaults`
/// builds them. A malformed `fallback_order` entry is dropped and named in
/// `warnings`.
///
/// The retention words a `[model_providers]` entry carries are not read,
/// since those entries are out of this loader's scope.
pub(super) fn model_defaults(settings: &Settings, warnings: &mut Vec<String>) -> ModelDefaults {
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
        fallback_model: settings
            .fallback_model
            .clone()
            .or_else(|| settings.default_model.clone()),
        fallback_order,
        provider_order: settings.providers.provider_order.clone(),
        retention: leviath_providers::retention::RetentionSettings {
            zero_requested: settings.providers.zero_retention,
            agreements: settings.providers.zero_retention_agreements.clone(),
            model_overrides: settings
                .model_capabilities
                .iter()
                .filter_map(|(model, caps)| caps.retention.map(|r| (model.clone(), r)))
                .collect(),
            provider_declarations: HashMap::new(),
            request_knob_aliases: HashMap::new(),
            file_uploads: settings.providers.file_uploads,
        },
    }
}
