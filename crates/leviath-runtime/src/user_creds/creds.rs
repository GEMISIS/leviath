//! Turning [`UserSettings`] into the providers a registry is built from.
//!
//! The one place this is done: `lev` calls [`provider_creds`] with the
//! settings it copied out of its own config, and
//! [`load_user_creds`](super::load_user_creds) with the ones it read from the
//! file.

use super::settings::{UserSettings, slots};
use crate::provider_creds::ProviderCreds;
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
///
/// The map is `String -> String`, so the enum has to be named somehow; using
/// the same spelling the TOML uses keeps one vocabulary rather than two.
fn cache_ttl_key(ttl: leviath_providers::anthropic::CacheTtl) -> &'static str {
    match ttl {
        leviath_providers::anthropic::CacheTtl::Ephemeral5m => "5m",
        leviath_providers::anthropic::CacheTtl::Ephemeral1h => "1h",
    }
}

/// Every provider `settings` opts in, in the order they are registered: the
/// keyed ones with a non-blank key, then Ollama, then each
/// `[model_providers]` endpoint by name, then Codex and Grok.
///
/// Every provider is opt-in: an API-keyed one when its key is set, an
/// endpoint when the file has its entry, a sign-in transport when it is
/// enabled, and Ollama when it was chosen or given an address.
///
/// `grants_path` is the file the sign-in grants are kept in. Without one the
/// sign-in providers are still listed, with no `auth_store_path` option, and
/// the registry skips them: where Leviath's files live is the caller's to
/// say, never guessed here.
pub fn provider_creds(settings: &UserSettings, grants_path: Option<&Path>) -> Vec<ProviderCreds> {
    let mut creds = Vec::new();
    for keyed in super::KEYED_PROVIDERS {
        let name = keyed.provider;
        let fields = slots(name);
        // A blank key is not a key: registering one produces a provider that
        // authenticates as nobody and fails at the first call.
        let Some(key) = non_blank((fields.key_ref)(settings).as_deref()) else {
            continue;
        };
        // The options map rather than a named field, for the reason it
        // exists: one provider's settings should not accrete onto every
        // provider's struct.
        let mut options = HashMap::new();
        if name == "anthropic"
            && let Some(ttl) = settings.providers.anthropic_cache_ttl
        {
            options.insert("cache_ttl".to_string(), cache_ttl_key(ttl).to_string());
        }
        // Only when set: the provider applies its own default otherwise, and
        // not writing it keeps settings that say nothing comparing equal to
        // themselves on reload.
        if name == leviath_providers::bedrock::PROVIDER_NAME
            && let Some(region) = non_blank(settings.providers.bedrock_region.as_deref())
        {
            options.insert("region".to_string(), region);
        }
        // The host this provider is reached on, when it is not the vendor's
        // own, and the extra headers that host wants on every request. Per
        // provider, because a gateway usually fronts one family and pointing
        // the others at it would break them.
        let base_url = non_blank((fields.base_url_ref)(settings).as_deref());
        let headers = (fields.headers)(settings)
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

    // Ollama is opt-in like every other provider, by the switch `lev setup`
    // writes or by naming an address. Needing no key and answering on a
    // well-known local port is not a reason to register it unasked: that
    // makes a bare model name in a blueprint resolvable against whatever
    // happens to be running locally, a place a run should end up only when
    // the user chose it.
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

    creds.extend(endpoint_creds(settings));

    // Codex needs no API key: its credential is a browser sign-in whose grant
    // lives outside the config entirely, so `api_key` stays `None` and the
    // provider reads the grant itself. Opt-in because registering it without
    // a sign-in would put a provider in the registry that cannot answer, and
    // because selecting it changes what gets billed.
    if settings.providers.codex_enabled {
        let name = leviath_providers::codex::PROVIDER_NAME;
        creds.push(ProviderCreds {
            name: name.to_string(),
            api_key: None,
            base_url: None,
            model_capabilities: settings.model_capabilities.clone(),
            // HTTP like the keyed providers, so the host-wide request
            // timeout applies.
            request_timeout_secs: settings.request_timeout_secs,
            rate_limit: settings.rate_limits.get(name).cloned(),
            options: codex_options(settings, grants_path),
        });
    }

    // Grok billed to a subscription: the xAI API over a browser sign-in, so it
    // takes xAI's host and headers, and the grant location a run reads.
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

/// Every `[model_providers.<name>]` entry that is an endpoint rather than a
/// script, in name order so the registry is built the same way from one
/// file whatever the map's iteration order was.
///
/// Registered natively, under its own name, so it needs no `.rhai` on disk
/// and lists its models like the built-ins do. A script entry is not this
/// function's: the host that runs scripts reads those.
fn endpoint_creds(settings: &UserSettings) -> Vec<ProviderCreds> {
    let mut endpoints: Vec<(&String, &super::ModelProviderConfig)> = settings
        .model_providers
        .iter()
        .filter(|(_, entry)| entry.is_endpoint())
        .collect();
    endpoints.sort_by(|a, b| a.0.cmp(b.0));

    let mut creds = Vec::new();
    for (name, entry) in endpoints {
        // `validate` refuses an endpoint without an address, so one is not in
        // settings read from a file; settings built by hand skip it rather
        // than register it pointing nowhere.
        let Some(base_url) = non_blank(entry.base_url.as_deref()) else {
            continue;
        };
        let mut cred = match (entry.is_openai(), non_blank(entry.api_key.as_deref())) {
            // OpenAI's own API at a host of its own. A deployment name does
            // not look like an OpenAI model id, so `models` routes here the
            // way `serves` does.
            (true, Some(key)) => ProviderCreds::openai_host(
                name.clone(),
                base_url,
                key,
                entry.header_pairs(),
                entry
                    .serves
                    .iter()
                    .flatten()
                    .chain(entry.models.iter().flatten())
                    .cloned()
                    .collect(),
            ),
            // Refused by `validate` too; skipped rather than registered with
            // nothing to authenticate with.
            (true, None) => continue,
            (false, api_key) => ProviderCreds::openai_compatible(
                name.clone(),
                base_url,
                api_key,
                entry.header_pairs(),
                entry.models.clone(),
                entry.serves.clone().unwrap_or_default(),
            ),
        }
        .with_auth_header(entry.auth_header());
        cred.model_capabilities = settings.model_capabilities.clone();
        cred.request_timeout_secs = settings.request_timeout_secs;
        // The entry's own limit, not `[rate_limits]`: that table is keyed by
        // the built-ins' names.
        cred.rate_limit = entry.rate_limit.clone();
        creds.push(cred);
    }
    creds
}

/// The `options` a browser sign-in provider's [`ProviderCreds`] carries, by
/// provider name: Codex's own, Grok's, or none for anything else.
///
/// Public for a credential check that has to build the provider the same way
/// a run does: it reads the grant from disk, so one that pointed somewhere
/// else would be checking a sign-in no run would ever use.
pub fn signin_options(
    settings: &UserSettings,
    grants_path: Option<&Path>,
    provider: &str,
) -> HashMap<String, String> {
    if provider == leviath_providers::codex::PROVIDER_NAME {
        codex_options(settings, grants_path)
    } else if provider == leviath_providers::grok::PROVIDER_NAME {
        grant_options(settings, grants_path)
    } else {
        HashMap::new()
    }
}

/// What every sign-in provider's options carry: where the grant is, and
/// whether it is in the OS credential store.
fn grant_options(settings: &UserSettings, grants_path: Option<&Path>) -> HashMap<String, String> {
    let mut options = HashMap::new();
    // Extended from an `Option` rather than branched on: with no path the
    // option is simply absent, and the registry skips the provider.
    options.extend(
        grants_path.map(|path| ("auth_store_path".to_string(), path.display().to_string())),
    );
    // A provider has no view of `[security]`, and a grant only its host could
    // read would leave the keychain backend silently signing a run out.
    if settings.security.credential_store == CredentialStoreKind::Keychain {
        options.insert("credential_store".to_string(), "keychain".to_string());
    }
    options
}

/// The `options` a Codex [`ProviderCreds`] carries.
fn codex_options(settings: &UserSettings, grants_path: Option<&Path>) -> HashMap<String, String> {
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
