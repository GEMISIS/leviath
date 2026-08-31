//! Config and models endpoints.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::types::*;
use crate::config::Config;

/// Every `[model_providers]` entry as the API reports it, name-sorted.
///
/// Sorted because the config holds them in a `HashMap`, whose iteration order
/// differs between two calls on one machine, and a list that reorders itself
/// under a form is a list nobody can edit.
fn gateways_of(c: &Config) -> Vec<GatewayInfo> {
    let mut gateways: Vec<GatewayInfo> = c
        .model_providers
        .iter()
        .map(|(name, p)| GatewayInfo {
            name: name.clone(),
            base_url: p.base_url.clone(),
            has_api_key: p.api_key.is_some(),
            kind: p.kind().as_str().to_string(),
            script: p.script.clone(),
            // Names only, for the reason `extra_keys` below gives.
            header_names: p.headers.iter().flatten().map(|(k, _)| k.clone()).collect(),
            models: p.models.clone().unwrap_or_default(),
            // Names only. See `GatewayInfo::extra_keys`: these values are
            // forwarded into a script and routinely hold credentials.
            extra_keys: {
                let mut keys: Vec<String> = p.extra.keys().cloned().collect();
                keys.sort();
                keys
            },
        })
        .collect();
    gateways.sort_by(|a, b| a.name.cmp(&b.name));
    gateways
}

/// Redacted view of a config: booleans for keys, never their values.
/// `requests` is what this server resolved at start-up, which no config
/// file alone can say once a flag is involved.
///
/// `health` is the file behind `c`, which on a broken save is not the file on
/// disk. Every other field here describes the config in force; without this
/// one a client had no way to tell "your edit is applied" from "your edit did
/// not parse and is being ignored".
fn redact(
    c: &Config,
    requests: &super::request_limits::RequestLimits,
    health: &crate::daemon::config_reload::ConfigHealth,
) -> RedactedConfig {
    let (config_error, config_mtime) = super::config_health::report(health);
    RedactedConfig {
        config_error,
        config_mtime,
        default_provider: c.default_provider.clone(),
        default_model: c.default_model.clone(),
        has_anthropic_key: c.providers.anthropic_api_key.is_some(),
        has_openai_key: c.providers.openai_api_key.is_some(),
        has_google_key: c.providers.google_api_key.is_some(),
        has_openrouter_key: c.openrouter_api_key.is_some(),
        ollama_base_url: c.ollama_base_url.clone(),
        // The switch or the address: either is a choice, and a console
        // drawing "Ollama is on" should not have to know which one was used.
        ollama_enabled: c.providers.ollama_enabled || c.ollama_base_url.is_some(),
        codex_enabled: c.providers.codex_enabled,
        codex_reasoning_effort: c.providers.codex_reasoning_effort.clone(),
        codex_verbosity: c.providers.codex_verbosity.clone(),
        codex_replay_reasoning: c.providers.codex_replay_reasoning,
        gateways: gateways_of(c),
        agent_paths: c.agent_paths.clone(),
        mcp_server_count: c.mcp_servers.len(),
        api_version: API_VERSION.to_string(),
        capabilities: API_CAPABILITIES.iter().map(|c| c.to_string()).collect(),
        limits: ApiLimits::current(requests),
    }
}

/// The reasoning efforts the Codex route accepts.
const CODEX_EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh"];

/// The text verbosities it accepts.
const CODEX_VERBOSITIES: &[&str] = &["low", "medium", "high"];

/// Refuse a value outside `allowed`, naming what was expected.
///
/// The provider drops a setting it does not recognise, so an unchecked typo
/// is saved, reported back by `GET /api/config`, and silently does nothing -
/// the same class of quiet no-op a mistyped `gate` key is in a blueprint.
fn validated(
    value: Option<String>,
    allowed: &[&str],
    what: &str,
) -> Result<Option<String>, ApiError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if allowed.contains(&value.as_str()) {
        return Ok(Some(value));
    }
    Err(err(
        StatusCode::BAD_REQUEST,
        format!(
            "unknown Codex {what} '{value}'; use one of {}",
            allowed.join(", ")
        ),
    ))
}

/// `GET /api/config`. Reads the file rather than a start-up copy, so an edit
/// made through [`put_config`] - or by anything else on the machine - is
/// visible to the very next request.
pub(super) async fn get_config(State(state): State<AppState>) -> Json<RedactedConfig> {
    // One `health()` call rather than a `current_config()` beside it: health
    // re-checks the file itself and hands back the config in force with its
    // verdict, so the two halves of one answer cannot disagree about whether a
    // save has been seen.
    let health = state.config.health();
    let config = health.config.clone();
    Json(redact(&config, &state.limits.request_limits, &health))
}

/// `PUT /api/config` (admin-only). Loads the on-disk config, applies every
/// present field, and writes it back with the file's `0600` permissions - the
/// same file `lev setup` and MCP admin edits. Returns the new redacted config.
pub(super) async fn put_config(
    State(state): State<AppState>,
    Json(req): Json<WriteConfigReq>,
) -> Result<Json<RedactedConfig>, ApiError> {
    let paths = super::mcp::admin_paths();
    let path = &paths.config;
    let mut config = Config::load_from_path_public(path).map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to read config: {e}"),
        )
    })?;

    if let Some(v) = req.default_provider {
        config.default_provider = v;
    }
    // Three states rather than the two every field around it has: absent
    // leaves the pin alone, `null` removes it so each blueprint picks its own
    // model again, and a string pins that one. Written as a `match` because
    // the read has to distinguish "the key was not sent" from "the key was
    // sent as null", which an `if let Some` on a single `Option` cannot.
    match req.default_model {
        None => {}
        Some(None) => config.default_model = None,
        // Refused rather than treated as a clear: `""` is not a model id, and
        // a form that posts its empty box should be told, not obeyed. The
        // check runs before anything is saved, so the file is untouched.
        Some(Some(v)) if v.trim().is_empty() => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "default_model must not be empty; send null to clear it".to_string(),
            ));
        }
        Some(Some(v)) => config.default_model = Some(v),
    }
    if let Some(v) = req.anthropic_key {
        config.providers.anthropic_api_key = Some(v);
    }
    if let Some(v) = req.openai_key {
        config.providers.openai_api_key = Some(v);
    }
    if let Some(v) = req.google_key {
        config.providers.google_api_key = Some(v);
    }
    if let Some(v) = req.openrouter_key {
        config.openrouter_api_key = Some(v);
    }
    if let Some(v) = req.ollama_base_url {
        config.ollama_base_url = Some(v);
    }
    // Checked before anything is written, like the gateway kind below: the
    // provider silently ignores a value it does not know, so a console that
    // sent a typo would see it saved and never take effect.
    let effort = validated(
        req.codex_reasoning_effort,
        CODEX_EFFORTS,
        "reasoning effort",
    )?;
    let verbosity = validated(req.codex_verbosity, CODEX_VERBOSITIES, "verbosity")?;
    config.providers.ollama_enabled = req
        .ollama_enabled
        .unwrap_or(config.providers.ollama_enabled);
    config.providers.codex_enabled = req.codex_enabled.unwrap_or(config.providers.codex_enabled);
    config.providers.codex_replay_reasoning = req
        .codex_replay_reasoning
        .unwrap_or(config.providers.codex_replay_reasoning);
    config.providers.codex_reasoning_effort = effort.or(config.providers.codex_reasoning_effort);
    config.providers.codex_verbosity = verbosity.or(config.providers.codex_verbosity);
    // Field by field, like everything above: a gateway names only what it is
    // changing, so a console can edit a base URL without knowing the key or
    // sending it back through the browser.
    for gateway in req.gateways.unwrap_or_default() {
        // Read before the entry is created, so a bad kind leaves the config
        // exactly as it was rather than with a half-made entry.
        let kind = match gateway.kind.as_deref() {
            None => None,
            Some(text) => Some(
                crate::config::ModelProviderKind::parse(text).ok_or_else(|| {
                    err(
                        StatusCode::BAD_REQUEST,
                        format!(
                            "gateway '{}': unknown kind '{text}'; use \"script\" or \
                             \"openai-compatible\"",
                            gateway.name
                        ),
                    )
                })?,
            ),
        };
        let entry = config.model_providers.entry(gateway.name).or_default();
        if let Some(v) = kind {
            entry.kind = Some(v);
        }
        if let Some(v) = gateway.base_url {
            entry.base_url = Some(v);
        }
        if let Some(v) = gateway.api_key {
            entry.api_key = Some(v);
        }
        if let Some(v) = gateway.script {
            entry.script = Some(v);
        }
        if let Some(v) = gateway.headers {
            entry.headers = Some(v);
        }
        if let Some(v) = gateway.models {
            entry.models = Some(v);
        }
    }
    // Removals run last, so one request that both edits and deletes cannot
    // depend on which half was applied first.
    for name in req.remove_gateways.unwrap_or_default() {
        config.model_providers.remove(&name);
    }
    // The same check the loader makes, made before the write: a file this
    // would refuse to read back is not a file worth saving.
    for (name, provider) in &config.model_providers {
        provider
            .validate(name)
            .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    }

    config.save_to_path_public(path).map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to write config: {e}"),
        )
    })?;
    // Health read *after* the write, so the answer describes the file this
    // request just left behind. A write this route made always parses - it
    // serializes a `Config` and the refusals above ran first - so this is
    // normally healthy; it is not assumed, because the file could equally have
    // been broken by hand a moment ago and rewritten by something else.
    let health = state.config.health();
    Ok(Json(redact(&config, &state.limits.request_limits, &health)))
}

/// `POST /api/models/probe` (admin-only): what an OpenAI-compatible server
/// at `base_url` says it serves, so a console can show the list and let a
/// person pick a default before the gateway is written.
///
/// Admin-gated with the write it precedes: it makes the serving host open a
/// connection to any address the caller names, which is the same category of
/// act as `POST /api/mcp/servers/{name}/test`.
pub(super) async fn probe_models(
    Json(req): Json<ProbeModelsReq>,
) -> Result<Json<ProbeModelsResp>, ApiError> {
    probe_models_with(req, &leviath_providers::provider::build_http_client).await
}

/// [`probe_models`], with client construction injected so the "no usable
/// HTTPS client" answer is reachable from a test.
pub(super) async fn probe_models_with(
    req: ProbeModelsReq,
    build_client: leviath_providers::provider::HttpClientFactory<'_>,
) -> Result<Json<ProbeModelsResp>, ApiError> {
    let (valid, message) = validate_base_url(&req.base_url);
    if !valid {
        return Err(err(StatusCode::BAD_REQUEST, message.unwrap_or_default()));
    }
    // The same provider a written gateway would get, built the same way, so
    // the probe cannot succeed where the gateway then fails.
    let mut creds = leviath_runtime::provider_creds::ProviderCreds::openai_compatible(
        "probe",
        req.base_url.trim(),
        req.api_key.filter(|k| !k.trim().is_empty()),
        req.headers.into_iter().flatten().collect(),
        None,
        Vec::new(),
    );
    creds.request_timeout_secs = Some(PROBE_TIMEOUT_SECS);
    let registry = leviath_runtime::provider_creds::build_provider_registry_with(
        std::slice::from_ref(&creds),
        build_client,
    )
    .map_err(|e| err(StatusCode::BAD_GATEWAY, e.to_string()))?;
    // Registered unconditionally under the name above: an endpoint cred
    // needs neither a key nor a reachable port to register.
    let provider = registry.get("probe").expect("an endpoint cred registers");
    let models = provider
        .list_models()
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let mut ids: Vec<String> = models.into_iter().map(|m| m.id).collect();
    ids.sort();
    Ok(Json(ProbeModelsResp { models: ids }))
}

/// How long the probe waits on the server. A person is watching a form.
const PROBE_TIMEOUT_SECS: u64 = 10;

/// Format-only validation of a provider key (no network call, no persistence).
fn validate_key_format(provider: &str, key: &str) -> (bool, Option<String>) {
    match provider {
        "anthropic" => {
            if key.starts_with("sk-ant-") {
                (true, None)
            } else {
                (
                    false,
                    Some("Anthropic keys start with `sk-ant-`.".to_string()),
                )
            }
        }
        "openai" => {
            if key.starts_with("sk-") {
                (true, None)
            } else {
                (false, Some("OpenAI keys start with `sk-`.".to_string()))
            }
        }
        "google" | "openrouter" => {
            if key.trim().is_empty() {
                (false, Some("Key must not be empty.".to_string()))
            } else {
                (true, None)
            }
        }
        // A custom gateway is custom precisely because its key has no house
        // format to check, so an unknown name is not a rejection: the only
        // thing that can be said about the key is that it is not empty.
        _ => match key.trim().is_empty() {
            true => (false, Some("Key must not be empty.".to_string())),
            false => (true, None),
        },
    }
}

/// Format-only check of a gateway's base URL.
///
/// Shape only, like the key check beside it: no request is made, because
/// `POST /api/config/validate` promises not to touch the network and a form
/// that hangs on an unreachable host is worse than one that says nothing. The
/// scheme is what people actually get wrong - a bare `api.example.com`, or an
/// `ollama serve` address pasted without one.
fn validate_base_url(url: &str) -> (bool, Option<String>) {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return (false, Some("Base URL must not be empty.".to_string()));
    }
    match trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        true => (true, None),
        false => (
            false,
            Some("Base URL must start with `http://` or `https://`.".to_string()),
        ),
    }
}

pub(super) async fn validate_config_key(Json(req): Json<ValidateKeyReq>) -> Json<ValidateKeyResp> {
    // The URL is checked first: a gateway with both wrong is more usefully
    // told about the address than about the key, since the key cannot be
    // judged beyond being present.
    if let Some(base_url) = &req.base_url {
        let (valid, message) = validate_base_url(base_url);
        if !valid {
            return Json(ValidateKeyResp { valid, message });
        }
    }
    let (valid, message) = validate_key_format(&req.provider, &req.key);
    Json(ValidateKeyResp { valid, message })
}

/// How long the models route waits for a provider to describe its own models.
///
/// Shorter than the daemon's start-up prime: this is a page load, and a limit
/// that arrives after the page has rendered is worth less than a fast answer
/// that admits which numbers are guesses.
const MODELS_PRIME_TIMEOUT_SECS: u64 = 5;

pub(super) async fn get_models(
    State(state): State<AppState>,
    Query(query): Query<ModelsQuery>,
) -> Json<Vec<ModelEntry>> {
    models_with(
        &state,
        &leviath_providers::provider::build_http_client,
        &query,
    )
    .await
}

/// [`get_models`], with client construction injected so the "no usable HTTPS
/// client" answer is reachable from a test.
pub(super) async fn models_with(
    state: &AppState,
    build_client: leviath_providers::provider::HttpClientFactory<'_>,
    query: &ModelsQuery,
) -> Json<Vec<ModelEntry>> {
    let mut models = list_models_from_config(&state.current_config(), build_client).await;
    // Filtered here rather than left to the caller because the interesting
    // case is two providers serving the *same* model ids: `openai` and
    // `codex` both answer to `gpt-5.5`, and they bill to different places.
    // A client that wants one of them should be able to ask for it rather
    // than fetch both and match on a string it had to know.
    if let Some(provider) = query.provider.as_deref() {
        models.retain(|m| m.provider == provider);
    }
    Json(models)
}

/// Every model every configured provider reports, as `provider/id`: what the
/// dashboard's agent editor offers once the providers have answered.
pub(crate) async fn list_model_ids(
    config: &crate::config::Config,
    build_client: leviath_providers::provider::HttpClientFactory<'_>,
) -> Vec<String> {
    list_models_from_config(config, build_client)
        .await
        .into_iter()
        .map(|m| format!("{}/{}", m.provider, m.id))
        .collect()
}

/// Every model every configured provider reports, for `GET /api/models` and
/// the dashboard's agent editor alike.
///
/// Iterates `resolvable_names` rather than `provider_names`, so a Rhai script
/// provider is asked too. `provider_names` returns natively registered
/// providers only, and a script provider is reachable through `get` alone, so
/// iterating that instead lists a script provider under the gateways while
/// offering none of its models - on the new-run page, in the agent editor and
/// in settings alike.
pub(super) async fn list_models_from_config(
    config: &crate::config::Config,
    build_client: leviath_providers::provider::HttpClientFactory<'_>,
) -> Vec<ModelEntry> {
    // Nothing to list if no client could be built; the endpoint answers with an
    // empty set rather than failing the request, matching how it treats a
    // provider whose `list_models` errors.
    let Ok(registry) = crate::commands::run::session::build_provider_registry_from_config_with(
        config,
        build_client,
    ) else {
        return Vec::new();
    };
    // Ask each provider what its models are before asking what they can hold.
    //
    // Without this the route published the compiled-in guess for every provider
    // whose real answer is a network call away - which is most of them, and
    // includes the two that had learned to fetch it. Measured against a running
    // server: Ollama reported a name-matched 131,072 for a model whose server
    // says 262,144, and OpenRouter reported builtin limits for all 418 of its
    // models. Only Google looked right, and only because its listing reads the
    // limits inline rather than through the primed table.
    //
    // The route already makes one network call per provider to list at all, so
    // this is a second bounded one, not a new class of cost. A provider that
    // does not answer in time keeps its compiled table and says so through
    // `limits_source`.
    registry
        .prime_capabilities(
            std::time::Duration::from_secs(MODELS_PRIME_TIMEOUT_SECS),
            &[],
        )
        .await;
    let mut models = Vec::new();

    for provider_name in registry.resolvable_names() {
        // A script name is a candidate until it compiles, so unlike the old
        // `provider_names` loop this cannot assume the lookup succeeds. A
        // script that will not load is skipped with its own log line already
        // written by the layer, exactly as a provider whose `list_models`
        // errors is skipped below.
        let Some(provider) = registry.get(&provider_name) else {
            continue;
        };
        if let Ok(list) = provider.list_models().await {
            for m in list {
                models.push(ModelEntry {
                    id: m.id,
                    provider: m.provider,
                    display_name: m.display_name,
                    max_context_tokens: m.capabilities.max_context_tokens,
                    max_output_tokens: m.capabilities.max_output_tokens,
                    limits_source: limits_source_label(m.capabilities.limits_source),
                    supports_tools: m.capabilities.supports_tools,
                    supports_temperature: m.capabilities.supports_temperature,
                    learned: m.learned,
                    released: m.released,
                    retires: m.retires,
                    pricing: m.pricing,
                });
            }
        }
    }

    models
}

/// The wire spelling of a [`LimitsSource`].
///
/// Written out here rather than serialized from the enum so the API's
/// vocabulary is visible at the boundary that publishes it: a rename in the
/// providers crate should not silently change what a console reads.
fn limits_source_label(source: leviath_providers::LimitsSource) -> String {
    match source {
        leviath_providers::LimitsSource::Api => "api",
        leviath_providers::LimitsSource::Builtin => "builtin",
        leviath_providers::LimitsSource::Override => "override",
    }
    .to_string()
}

#[cfg(test)]
mod limits_source_label_tests {
    use super::limits_source_label;
    use leviath_providers::LimitsSource;

    /// The three spellings a client switches on. Pinned here because they are
    /// the wire vocabulary: renaming the enum in the providers crate must not
    /// quietly change what a console reads, and only this test would notice.
    #[test]
    fn every_source_has_its_wire_spelling() {
        assert_eq!(limits_source_label(LimitsSource::Api), "api");
        assert_eq!(limits_source_label(LimitsSource::Builtin), "builtin");
        assert_eq!(limits_source_label(LimitsSource::Override), "override");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::serve::mcp::{AdminPaths, scoped};
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use std::sync::Arc;
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    use crate::commands::serve::events::ServerEvent;
    use crate::config::Config;

    /// The health of a reloader with no file to watch: loading, because there
    /// is nothing that could fail to. For the tests about the *other* fields
    /// `redact` fills in.
    fn healthy() -> crate::daemon::config_reload::ConfigHealth {
        crate::daemon::config_reload::ConfigReloader::fixed(Config::default()).health()
    }

    /// A config with a registered provider whose `list_models` cannot succeed,
    /// which is the `Err` arm of the handler's `if let Ok(list)`.
    ///
    /// A keyed provider is the reliable way to put something in the registry
    /// that will then fail to list: the key registers it, and nothing is
    /// listening at the address it calls. Ollama will not do, because it
    /// registers only when something answers there, so a dead address leaves
    /// no provider at all and the arm goes unrun. Port 1 is reserved and never
    /// bound, so the result does not depend on what happens to be running on
    /// the developer's machine.
    fn state_without_a_reachable_ollama() -> AppState {
        let (tx, _) = broadcast::channel::<ServerEvent>(64);
        AppState {
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config {
                ollama_base_url: Some("http://127.0.0.1:1".to_string()),
                providers: crate::config::ProviderConfig {
                    anthropic_api_key: Some("test-key".to_string()),
                    anthropic_base_url: Some("http://127.0.0.1:1".to_string()),
                    ..Config::default().providers
                },
                ..Config::default()
            }),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            providers: crate::commands::serve::providers::ProviderAdmin::default(),
            limits: Default::default(),
        }
    }

    /// `?provider=` narrows the listing, which is what makes two providers
    /// serving the same model id tellable apart.
    ///
    /// That is the real case, not a hypothetical one: `openai` and `codex`
    /// both answer to `gpt-5.5` and bill to entirely different places. This
    /// stands two gateways up at dead ports with the same model in each, so
    /// the ids collide exactly the way those two do - a client keying on `id`
    /// alone would collapse them into one row and whichever it kept would
    /// decide what the user pays.
    #[tokio::test]
    async fn the_model_listing_can_be_narrowed_to_one_provider() {
        use crate::config::{ModelProviderConfig, ModelProviderKind};

        let mut config = Config::default();
        for name in ["alpha", "zeta"] {
            config.model_providers.insert(
                name.to_string(),
                ModelProviderConfig {
                    kind: Some(ModelProviderKind::OpenaiCompatible),
                    // Nothing listens, so each falls back to the models its
                    // entry names rather than asking a server.
                    base_url: Some("http://127.0.0.1:1/v1".to_string()),
                    models: Some(vec!["shared-model".to_string()]),
                    ..Default::default()
                },
            );
        }
        let (tx, _) = broadcast::channel::<ServerEvent>(64);
        let state = AppState {
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(config),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            providers: crate::commands::serve::providers::ProviderAdmin::default(),
            limits: Default::default(),
        };
        let build = leviath_providers::provider::build_http_client;

        // Unfiltered: the same id under both providers.
        let Json(all) = super::models_with(&state, &build, &Default::default()).await;
        // Sorted: the registry's iteration order is not the contract here,
        // the pair of entries under one id is.
        let mut shared: Vec<&str> = all
            .iter()
            .filter(|m| m.id == "shared-model")
            .map(|m| m.provider.as_str())
            .collect();
        shared.sort_unstable();
        let listed: Vec<String> = all
            .iter()
            .map(|m| format!("{}/{}", m.provider, m.id))
            .collect();
        assert_eq!(shared, ["alpha", "zeta"], "{listed:?}");

        // Filtered: one of them, and the id is still there.
        let Json(narrowed) = super::models_with(
            &state,
            &build,
            &super::ModelsQuery {
                provider: Some("zeta".to_string()),
            },
        )
        .await;
        let got: Vec<String> = narrowed
            .iter()
            .map(|m| format!("{}/{}", m.provider, m.id))
            .collect();
        assert_eq!(narrowed.len(), 1, "{got:?}");
        assert_eq!(narrowed[0].provider, "zeta");
        assert_eq!(narrowed[0].id, "shared-model");

        // A provider this machine does not have lists nothing, rather than
        // erroring: "no models" is the honest answer.
        let Json(none) = super::models_with(
            &state,
            &build,
            &super::ModelsQuery {
                provider: Some("not-a-provider".to_string()),
            },
        )
        .await;
        assert!(none.is_empty());
    }

    fn test_state() -> AppState {
        let (tx, _) = broadcast::channel::<ServerEvent>(64);
        AppState {
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config::default()),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            providers: crate::commands::serve::providers::ProviderAdmin::default(),
            limits: Default::default(),
        }
    }

    fn test_state_with_keys() -> AppState {
        let (tx, _) = broadcast::channel::<ServerEvent>(64);
        AppState {
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config {
                providers: crate::config::ProviderConfig {
                    anthropic_api_key: Some("sk-ant-test".to_string()),
                    openai_api_key: Some("sk-openai-test".to_string()),
                    google_api_key: None,
                    anthropic_base_url: None,
                    openai_base_url: None,
                    google_base_url: None,
                    openrouter_base_url: None,
                    claude_code_enabled: false,
                    claude_code_binary: None,
                    claude_code_effort: None,
                    anthropic_cache_ttl: None,
                    fallback_order: Vec::new(),
                    ..Default::default()
                },
                openrouter_api_key: Some("sk-or-test".to_string()),
                ollama_base_url: Some("http://localhost:11434".to_string()),
                mcp_servers: vec![],
                ..Default::default()
            }),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            providers: crate::commands::serve::providers::ProviderAdmin::default(),
            limits: Default::default(),
        }
    }

    // ─── get_config endpoint ──────────────────────────────────────────────────

    #[tokio::test]
    async fn get_config_default_returns_ok() {
        let app = Router::new()
            .route("/api/config", get(get_config))
            .with_state(test_state());
        let req = Request::builder()
            .uri("/api/config")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let config: RedactedConfig = serde_json::from_slice(&body).unwrap();
        assert_eq!(config.default_provider, "anthropic");
        assert!(!config.has_anthropic_key);
        assert!(!config.has_openai_key);
        assert!(!config.has_openrouter_key);
        assert!(config.ollama_base_url.is_none());
    }

    /// A config that does not parse is reported alongside the last good one.
    /// The config on its own reads exactly like an edit that was never made,
    /// so the error is the only thing that tells the two apart.
    #[tokio::test]
    async fn get_config_reports_a_broken_file_and_keeps_serving_the_last_good_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let good = crate::config::Config {
            default_provider: "google".to_string(),
            ..Default::default()
        };
        write_config(&path, &toml::to_string(&good).unwrap());
        let state = crate::commands::serve::testutil::state_with_config_at(&path);

        // While it loads: no error, and an mtime for the config in force.
        let config = fetch_config(&state).await;
        assert!(config.config_error.is_none());
        let good_mtime = config.config_mtime;
        assert!(good_mtime.is_some());

        write_config(&path, "default_provider = \"google\"\nbroken : :\n");
        let config = fetch_config(&state).await;

        let error = config.config_error.expect("the file no longer loads");
        assert_eq!(error.kind, "parse");
        assert_eq!(error.line, Some(2));
        assert_eq!(error.column, Some(8));
        assert_eq!(error.path, path.display().to_string());
        assert!(
            error.note.contains("last one that loaded"),
            "{}",
            error.note
        );
        assert_eq!(
            config.config_mtime, good_mtime,
            "the mtime is the config in force, not the file that failed"
        );
        assert_eq!(
            config.default_provider, "google",
            "the last good config is still what is served"
        );

        // Fixed: the error goes away by itself, with no restart.
        write_config(&path, "default_provider = \"openai\"\n");
        let config = fetch_config(&state).await;
        assert!(config.config_error.is_none());
        assert_eq!(config.default_provider, "openai");
    }

    /// Write a config file with an mtime strictly newer than the last write,
    /// so the reloader sees a save even inside one clock tick.
    fn write_config(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
    }

    /// `GET /api/config` against `state`, parsed.
    async fn fetch_config(state: &AppState) -> RedactedConfig {
        let app = Router::new()
            .route("/api/config", get(get_config))
            .with_state(state.clone());
        let req = Request::builder()
            .uri("/api/config")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    /// Without these a console feature-detects by asking for something and
    /// reading a 404 as "unsupported" - which is also what a missing run looks
    /// like, and costs one round trip per feature.
    #[tokio::test]
    async fn get_config_advertises_the_api_version_capabilities_and_limits() {
        let app = Router::new()
            .route("/api/config", get(get_config))
            .with_state(test_state());
        let req = Request::builder()
            .uri("/api/config")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let config: RedactedConfig = serde_json::from_slice(&body).unwrap();

        assert_eq!(config.api_version, API_VERSION);
        for expected in [
            "runs.envelope",
            "runs.search",
            "runs.files.listing",
            "blueprints.envelope",
            "blueprints.fan_outs",
            "context.history.page",
            // The console decides whether to draw its "New Folder" button on
            // this string, so a build that serves `POST /api/fs/dirs` without
            // saying so is a button nobody gets.
            "fs.mkdir",
        ] {
            assert!(
                config.capabilities.iter().any(|c| c == expected),
                "missing capability {expected}"
            );
        }

        // The numbers are what make this useful rather than decorative: a
        // client that knows a feature exists still has to know its caps, and
        // every one it guesses would be hardcoded and eventually wrong.
        assert_eq!(
            config.limits.max_limit,
            crate::commands::serve::runs::MAX_LIMIT
        );
        assert_eq!(
            config.limits.max_file_bytes,
            crate::commands::serve::agents::MAX_FILE_READ_BYTES
        );
        assert_eq!(
            config.limits.max_tracked_modified_files,
            leviath_core::run_meta::MAX_TRACKED_MODIFIED_FILES
        );
    }

    #[tokio::test]
    async fn get_config_with_keys_shows_has_key_true() {
        let app = Router::new()
            .route("/api/config", get(get_config))
            .with_state(test_state_with_keys());
        let req = Request::builder()
            .uri("/api/config")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let config: RedactedConfig = serde_json::from_slice(&body).unwrap();
        assert!(config.has_anthropic_key);
        assert!(config.has_openai_key);
        assert!(config.has_openrouter_key);
        assert_eq!(
            config.ollama_base_url.as_deref(),
            Some("http://localhost:11434")
        );
        // Must not contain actual key values
        let raw = std::str::from_utf8(&body).unwrap();
        assert!(!raw.contains("sk-ant-test"));
        assert!(!raw.contains("sk-openai-test"));
    }

    #[tokio::test]
    async fn get_config_agent_paths_included() {
        let (tx, _) = broadcast::channel::<ServerEvent>(64);
        let state = AppState {
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config {
                agent_paths: vec![
                    std::path::PathBuf::from("/my/agents"),
                    std::path::PathBuf::from("/other/agents"),
                ],
                ..Default::default()
            }),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            providers: crate::commands::serve::providers::ProviderAdmin::default(),
            limits: Default::default(),
        };
        let app = Router::new()
            .route("/api/config", get(get_config))
            .with_state(state);
        let req = Request::builder()
            .uri("/api/config")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let config: RedactedConfig = serde_json::from_slice(&body).unwrap();
        assert_eq!(config.agent_paths.len(), 2);
    }

    // ─── get_models endpoint ──────────────────────────────────────────────────

    /// AppState whose registry has a provider that actually enumerates models,
    /// so the `/api/models` handler's list-building loop runs. `claude-code`
    /// needs no API key and `list_models` returns its three known models.
    fn test_state_listing_models() -> AppState {
        let (tx, _) = broadcast::channel::<ServerEvent>(64);
        AppState {
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config {
                providers: crate::config::ProviderConfig {
                    anthropic_base_url: None,
                    openai_base_url: None,
                    google_base_url: None,
                    openrouter_base_url: None,
                    claude_code_enabled: true,
                    ..Config::default().providers
                },
                ..Config::default()
            }),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            providers: crate::commands::serve::providers::ProviderAdmin::default(),
            limits: Default::default(),
        }
    }

    #[tokio::test]
    async fn get_models_returns_ok() {
        // A reachable ollama would make `list_models` succeed and leave the
        // other arm of `if let Ok(list)` unrun - see `state_without_a_reachable_ollama`.
        let app = Router::new()
            .route("/api/models", get(get_models))
            .with_state(state_without_a_reachable_ollama());
        let req = Request::builder()
            .uri("/api/models")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let models: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        // With default config (no API keys, claude-code off), providers may
        // return empty lists, but the endpoint itself should succeed.
        let _ = models;
    }

    #[tokio::test]
    async fn get_models_enumerates_when_a_provider_lists_models() {
        let app = Router::new()
            .route("/api/models", get(get_models))
            .with_state(test_state_listing_models());
        let req = Request::builder()
            .uri("/api/models")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let models: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        // claude-code enumerates its three known models, so the handler's
        // per-model mapping loop actually runs and produces entries.
        assert!(!models.is_empty());
        assert!(models.iter().any(|m| m["provider"] == "claude-code"));
        assert!(models.iter().all(|m| m["id"].is_string()));
        // The fields The Lair reads to describe a model beyond its size: the
        // shape flags, whether the row is the provider's own answer, and the
        // dates and rates a listing carries when it has them.
        for m in &models {
            assert!(m["supports_temperature"].is_boolean(), "{m}");
            assert!(m["supports_tools"].is_boolean(), "{m}");
            assert!(m["learned"].is_boolean(), "{m}");
            assert!(m.get("released").is_some(), "{m}");
            assert!(m.get("retires").is_some(), "{m}");
            assert!(m.get("pricing").is_some(), "{m}");
        }
    }

    /// A script provider's models must reach `GET /api/models`, or The Lair
    /// lists the gateway under settings while offering none of its models on
    /// the new-run page or in the agent editor.
    ///
    /// A real `.rhai` on disk under an isolated `LEVIATH_HOME`, not a mock:
    /// the endpoint builds its own registry, so the only way to put a script
    /// provider in front of it is to put one where it looks.
    #[tokio::test]
    async fn api_models_includes_a_script_providers_models() {
        let home = tempfile::tempdir().unwrap();
        let providers = home.path().join(".leviath").join("providers");
        std::fs::create_dir_all(&providers).unwrap();
        std::fs::write(
            providers.join("scripted.rhai"),
            "fn initialize(config) { #{ ok: true } }\n\
             fn inference(state, request) { #{ content: \"ok\" } }\n\
             fn list_models(state) { [ #{ id: \"scripted-large\", \
             max_context_tokens: 32768 } ] }",
        )
        .unwrap();
        // A second script that will not compile: it must be skipped rather
        // than take the endpoint down with it.
        std::fs::write(providers.join("broken.rhai"), "fn initialize(config) { #{").unwrap();

        let models = temp_env::async_with_vars(
            [("LEVIATH_HOME", Some(home.path()))],
            list_models_from_config(
                &Config::default(),
                &leviath_providers::provider::build_http_client,
            ),
        )
        .await;

        let scripted: Vec<_> = models.iter().filter(|m| m.provider == "scripted").collect();
        let named: Vec<&str> = models.iter().map(|m| m.provider.as_str()).collect();
        assert_eq!(scripted.len(), 1, "the script provider answered: {named:?}");
        assert_eq!(scripted[0].id, "scripted-large");
        assert_eq!(scripted[0].max_context_tokens, 32768);
        assert!(
            !models.iter().any(|m| m.provider == "broken"),
            "a script that will not compile is skipped, not fatal"
        );
    }

    /// The dashboard's flat `provider/id` list is the same enumeration.
    #[tokio::test]
    async fn list_model_ids_flattens_to_provider_slash_id() {
        let state = test_state_listing_models();
        let ids = super::list_model_ids(
            &state.current_config(),
            &leviath_providers::provider::build_http_client,
        )
        .await;
        assert!(!ids.is_empty());
        assert!(
            ids.iter().any(|id| id.starts_with("claude-code/")),
            "{ids:?}"
        );
    }

    #[test]
    fn redacted_config_hides_keys() {
        let config = RedactedConfig {
            default_provider: "anthropic".to_string(),
            default_model: None,
            has_anthropic_key: true,
            has_openai_key: false,
            has_google_key: false,
            has_openrouter_key: false,
            ollama_base_url: None,
            ollama_enabled: false,
            codex_enabled: false,
            codex_reasoning_effort: None,
            codex_verbosity: None,
            codex_replay_reasoning: true,
            gateways: Vec::new(),
            agent_paths: vec![],
            mcp_server_count: 2,
            api_version: API_VERSION.to_string(),
            capabilities: API_CAPABILITIES.iter().map(|c| c.to_string()).collect(),
            limits: ApiLimits::current(&Default::default()),
            config_error: None,
            config_mtime: None,
        };
        let json = serde_json::to_string(&config).unwrap();
        // Must NOT contain actual key values
        assert!(!json.contains("sk-"));
        assert!(json.contains("\"has_anthropic_key\":true"));
        assert!(json.contains("\"has_openai_key\":false"));
        assert!(json.contains("\"mcp_server_count\":2"));
    }

    #[test]
    fn redacted_config_with_ollama_url() {
        let config = RedactedConfig {
            default_provider: "ollama".to_string(),
            default_model: None,
            has_anthropic_key: false,
            has_openai_key: false,
            has_google_key: false,
            has_openrouter_key: false,
            ollama_base_url: Some("http://localhost:11434".to_string()),
            ollama_enabled: false,
            codex_enabled: false,
            codex_reasoning_effort: None,
            codex_verbosity: None,
            codex_replay_reasoning: true,
            gateways: Vec::new(),
            agent_paths: vec![],
            mcp_server_count: 0,
            api_version: API_VERSION.to_string(),
            capabilities: API_CAPABILITIES.iter().map(|c| c.to_string()).collect(),
            limits: ApiLimits::current(&Default::default()),
            config_error: None,
            config_mtime: None,
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"ollama_base_url\":\"http://localhost:11434\""));
    }

    // ─── put_config endpoint ──────────────────────────────────────────────────

    /// The state and the file locations a `put_config` test runs against.
    fn state_with_config_path(path: std::path::PathBuf) -> (AppState, AdminPaths) {
        let (tx, _) = broadcast::channel::<ServerEvent>(64);
        let state = AppState {
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config::default()),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            providers: crate::commands::serve::providers::ProviderAdmin::default(),
            limits: Default::default(),
        };
        (state, paths_for(path))
    }

    fn paths_for(config: std::path::PathBuf) -> AdminPaths {
        let store = config.with_file_name("mcp-auth.json");
        let grants = config.with_file_name("provider-auth.json");
        AdminPaths {
            config,
            store,
            grants,
        }
    }

    /// [`state_with_config_path`], but its config source *watches* that file
    /// rather than holding a copy - the way `lev serve` builds one. What the
    /// handlers see is then whatever is on disk.
    fn state_watching_config_path(path: std::path::PathBuf) -> (AppState, AdminPaths) {
        let (tx, _) = broadcast::channel::<ServerEvent>(64);
        let state = AppState {
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: Arc::new(crate::daemon::config_reload::ConfigReloader::new(
                path.clone(),
                Config::default(),
            )),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            providers: crate::commands::serve::providers::ProviderAdmin::default(),
            limits: Default::default(),
        };
        (state, paths_for(path))
    }

    /// Force a file's mtime strictly newer, so the reload is observable even
    /// when the write lands in the same clock tick as the last one (mirrors
    /// `config_reload`'s own test helper).
    fn bump_mtime(path: &std::path::Path) {
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(later).unwrap();
    }

    /// The endpoint's answer as JSON rather than as `RedactedConfig`: a
    /// gateway serializes with `skip_serializing_if` fields that its own
    /// `Deserialize` requires, so the wire form is the only faithful reading.
    async fn get_config_request((state, paths): (AppState, AdminPaths)) -> serde_json::Value {
        let app = scoped(
            Router::new()
                .route("/api/config", get(get_config))
                .with_state(state),
            paths,
        );
        let req = Request::builder()
            .uri("/api/config")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).expect("the endpoint answers with JSON")
    }

    /// The round trip: save an edit, reload the page, and the edit is still
    /// there. `put_config` writes the file, so `get_config` has to read the
    /// file back and not a start-up copy, or the save reads as lost.
    #[tokio::test]
    async fn an_edit_through_put_is_visible_to_the_next_get() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_to_path_public(&path).unwrap();
        let state = state_watching_config_path(path.clone());

        let before = get_config_request(state.clone()).await;
        assert_ne!(before["default_provider"], "openai");

        let body = serde_json::json!({ "default_provider": "openai" }).to_string();
        let resp = put_config_request(state.clone(), &body).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        bump_mtime(&path);

        let after = get_config_request(state).await;
        assert_eq!(
            after["default_provider"], "openai",
            "the edit this server just wrote must be what it reports"
        );
    }

    /// And an edit made by anything else on the machine - `lev setup`, an
    /// editor, the daemon - is picked up the same way. `lev serve` is a
    /// separate process, so a daemon restart never fixed this one.
    #[tokio::test]
    async fn an_edit_made_outside_the_api_is_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_to_path_public(&path).unwrap();
        let state = state_watching_config_path(path.clone());
        let before = get_config_request(state.clone()).await;
        assert_eq!(before["gateways"].as_array().map(Vec::len), Some(0));

        let mut edited = Config::default();
        edited.model_providers.insert(
            "cerebras".to_string(),
            crate::config::ModelProviderConfig {
                base_url: Some("https://api.cerebras.ai/v1".to_string()),
                ..Default::default()
            },
        );
        edited.save_to_path_public(&path).unwrap();
        bump_mtime(&path);

        let after = get_config_request(state).await;
        let gateways = after["gateways"].as_array().expect("a gateway array");
        assert_eq!(gateways.len(), 1, "the new gateway is reported");
        assert_eq!(gateways[0]["name"], "cerebras");
        assert_eq!(gateways[0]["base_url"], "https://api.cerebras.ai/v1");
    }

    async fn put_config_request(
        (state, paths): (AppState, AdminPaths),
        body: &str,
    ) -> axum::http::Response<Body> {
        let app = scoped(
            Router::new()
                .route("/api/config", axum::routing::put(put_config))
                .with_state(state),
            paths,
        );
        let req = Request::builder()
            .method("PUT")
            .uri("/api/config")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        app.oneshot(req).await.unwrap()
    }

    /// Ollama is writable and readable, so a console can turn it on.
    ///
    /// Found by driving the live API rather than by reading the code: the
    /// Codex switch was on `GET`/`PUT` and this one was not, so a settings
    /// page could see `ollama_base_url` and had no way to learn - or say -
    /// whether Ollama was on at all.
    #[tokio::test]
    async fn put_config_writes_the_ollama_switch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_to_path_public(&path).unwrap();

        let body = serde_json::json!({ "ollama_enabled": true }).to_string();
        let resp = put_config_request(state_with_config_path(path.clone()), &body).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let rc: RedactedConfig = serde_json::from_slice(&bytes).unwrap();
        assert!(rc.ollama_enabled);
        assert!(
            Config::load_from_path_public(&path)
                .unwrap()
                .providers
                .ollama_enabled
        );
    }

    /// An address counts as having chosen it, so the switch reads true for a
    /// config that predates the switch. A console asking "is Ollama on" wants
    /// one answer, not two fields to reconcile.
    #[tokio::test]
    async fn an_ollama_address_reads_as_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_to_path_public(&path).unwrap();

        // The *watching* state, built first and then edited underneath it:
        // `state_with_config_path` serves a fixed config, and the reloader is
        // mtime-checked, so the edit has to land after construction and look
        // newer. This is the dance every other test of this reloader does.
        let state = state_watching_config_path(path.clone());
        Config {
            ollama_base_url: Some("http://elsewhere:11434".to_string()),
            ..Config::default()
        }
        .save_to_path_public(&path)
        .unwrap();
        bump_mtime(&path);

        let json = get_config_request(state).await;
        assert_eq!(json["ollama_enabled"], true, "{json}");
    }

    /// The Codex switch and its two tuning knobs are writable, so a console
    /// can offer the provider without the user editing `config.toml` by hand.
    #[tokio::test]
    async fn put_config_writes_the_codex_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_to_path_public(&path).unwrap();

        let body = serde_json::json!({
            "codex_enabled": true,
            "codex_reasoning_effort": "high",
            "codex_verbosity": "low",
            "codex_replay_reasoning": false,
        })
        .to_string();
        let resp = put_config_request(state_with_config_path(path.clone()), &body).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let rc: RedactedConfig = serde_json::from_slice(&bytes).unwrap();
        assert!(rc.codex_enabled);
        assert_eq!(rc.codex_reasoning_effort.as_deref(), Some("high"));
        assert_eq!(rc.codex_verbosity.as_deref(), Some("low"));
        assert!(!rc.codex_replay_reasoning);

        let saved = Config::load_from_path_public(&path).unwrap();
        assert!(saved.providers.codex_enabled);
        assert_eq!(
            saved.providers.codex_reasoning_effort.as_deref(),
            Some("high")
        );
        assert_eq!(saved.providers.codex_verbosity.as_deref(), Some("low"));
        assert!(!saved.providers.codex_replay_reasoning);
    }

    /// An absent field leaves the setting alone, the partial-update rule every
    /// field here follows. Without it a console editing the effort would turn
    /// the provider off by not mentioning it.
    #[tokio::test]
    async fn a_put_that_mentions_no_codex_field_changes_none_of_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::default();
        config.providers.codex_enabled = true;
        config.providers.codex_verbosity = Some("high".to_string());
        config.save_to_path_public(&path).unwrap();

        let body = serde_json::json!({ "default_provider": "codex" }).to_string();
        let resp = put_config_request(state_with_config_path(path.clone()), &body).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let saved = Config::load_from_path_public(&path).unwrap();
        assert!(saved.providers.codex_enabled, "the switch was turned off");
        assert_eq!(saved.providers.codex_verbosity.as_deref(), Some("high"));
    }

    /// A value the provider would ignore is refused rather than saved. The
    /// route drops an effort it does not recognise, so an unchecked typo is
    /// written, read back by `GET /api/config`, and quietly does nothing.
    #[tokio::test]
    async fn a_codex_setting_the_provider_would_ignore_is_refused() {
        for (field, value) in [
            ("codex_reasoning_effort", "hard"),
            ("codex_verbosity", "verbose"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            Config::default().save_to_path_public(&path).unwrap();

            let body = serde_json::json!({ field: value, "codex_enabled": true }).to_string();
            let resp = put_config_request(state_with_config_path(path.clone()), &body).await;
            assert_eq!(
                resp.status(),
                axum::http::StatusCode::BAD_REQUEST,
                "{field} = {value}"
            );

            let saved = Config::load_from_path_public(&path).unwrap();
            assert!(
                !saved.providers.codex_enabled,
                "the check runs before anything is written, so {field} left the file alone"
            );
        }
    }

    #[tokio::test]
    async fn put_config_writes_all_present_fields_and_redacts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_to_path_public(&path).unwrap();

        let body = serde_json::json!({
            "default_provider": "openai",
            "default_model": "gpt-5",
            "anthropic_key": "sk-ant-x",
            "openai_key": "sk-openai-x",
            "google_key": "g-x",
            "openrouter_key": "or-x",
            "ollama_base_url": "http://ollama:11434"
        })
        .to_string();
        let resp = put_config_request(state_with_config_path(path.clone()), &body).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let raw = std::str::from_utf8(&bytes).unwrap();
        assert!(!raw.contains("sk-ant-x"), "must not leak key values");
        let rc: RedactedConfig = serde_json::from_slice(&bytes).unwrap();
        assert!(
            rc.has_anthropic_key && rc.has_openai_key && rc.has_google_key && rc.has_openrouter_key
        );
        assert_eq!(rc.default_provider, "openai");

        let saved = Config::load_from_path_public(&path).unwrap();
        assert_eq!(
            saved.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-x")
        );
        assert_eq!(
            saved.providers.openai_api_key.as_deref(),
            Some("sk-openai-x")
        );
        assert_eq!(saved.providers.google_api_key.as_deref(), Some("g-x"));
        assert_eq!(saved.openrouter_api_key.as_deref(), Some("or-x"));
        assert_eq!(saved.default_model.as_deref(), Some("gpt-5"));
        assert_eq!(
            rc.default_model.as_deref(),
            Some("gpt-5"),
            "the answer reports the pin it just wrote"
        );
        assert_eq!(
            saved.ollama_base_url.as_deref(),
            Some("http://ollama:11434")
        );
    }

    /// A config with `default_model` pinned to `gpt-5`, saved at `path`.
    ///
    /// A named helper rather than a closure at each call site: a closure is a
    /// separate region per site, and four of them are four uncovered regions
    /// the day one test stops running.
    fn pinned_config_at(path: &std::path::Path) {
        let pinned = Config {
            default_model: Some("gpt-5".to_string()),
            ..Default::default()
        };
        pinned.save_to_path_public(path).unwrap();
    }

    /// `GET /api/config` says which model is pinned, and says `null` out loud
    /// when none is.
    ///
    /// Serialized unconditionally, like `ollama_base_url` beside it, because
    /// the third case matters: a daemon too old to report this omits the key
    /// entirely, and a console has to read that as "cannot say" rather than
    /// as "nothing is set". Collapsing the two is what left the picker
    /// drawing an empty box over a machine with a model pinned.
    #[tokio::test]
    async fn get_config_reports_the_default_model_when_set_and_null_when_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_to_path_public(&path).unwrap();
        let state = state_watching_config_path(path.clone());

        let unset = get_config_request(state.clone()).await;
        assert!(
            unset.get("default_model").is_some(),
            "the key is always there, so its absence can mean something else"
        );
        assert!(
            unset["default_model"].is_null(),
            "nothing pinned reads as null"
        );

        pinned_config_at(&path);
        bump_mtime(&path);

        let set = get_config_request(state).await;
        assert_eq!(set["default_model"], "gpt-5");
    }

    /// The partial-update rule the rest of the body follows: a key this
    /// request does not mention is left exactly as it was.
    #[tokio::test]
    async fn put_config_without_default_model_leaves_the_pin_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        pinned_config_at(&path);

        let body = serde_json::json!({ "default_provider": "openai" }).to_string();
        let resp = put_config_request(state_with_config_path(path.clone()), &body).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let saved = Config::load_from_path_public(&path).unwrap();
        assert_eq!(
            saved.default_model.as_deref(),
            Some("gpt-5"),
            "an absent key changes nothing"
        );
        assert_eq!(saved.default_provider, "openai");
    }

    /// `null` is the other door: it writes the setting away, so every stage
    /// goes back to the model its blueprint names.
    ///
    /// The file is what is checked, not the struct in memory. "Cleared" has
    /// to survive the save: a `None` that still serialized a `default_model`
    /// line would read back as pinned on the next load, and the console would
    /// see its own clear undone one request later.
    #[tokio::test]
    async fn put_config_with_null_clears_the_default_model_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        pinned_config_at(&path);
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("gpt-5"),
            "the pin starts out on disk"
        );

        let body = serde_json::json!({ "default_model": null }).to_string();
        let resp = put_config_request(state_with_config_path(path.clone()), &body).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let answer: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            answer["default_model"].is_null(),
            "the answer already reports it gone"
        );

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("default_model"),
            "the key is gone from the file, not merely None in memory"
        );
        let reread = Config::load_from_path_public(&path).unwrap();
        assert_eq!(reread.default_model, None, "and it stays gone on re-read");
    }

    /// An empty string is not a clear and not a model id, so it is a 400.
    ///
    /// The alternative, reading `""` as "unset", makes a form that posts its
    /// empty box silently destroy a setting - and there is a spelling for
    /// that already, one character long. Refused the way an
    /// `openai-compatible` gateway with an empty `base_url` is refused, and
    /// like that one it is refused before anything is written.
    #[tokio::test]
    async fn put_config_refuses_an_empty_default_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        pinned_config_at(&path);

        for body in [r#"{"default_model": ""}"#, r#"{"default_model": "   "}"#] {
            let resp = put_config_request(state_with_config_path(path.clone()), body).await;
            assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
            let saved = Config::load_from_path_public(&path).unwrap();
            assert_eq!(
                saved.default_model.as_deref(),
                Some("gpt-5"),
                "a refused write leaves the file as it was"
            );
        }
    }

    #[tokio::test]
    async fn put_config_empty_body_leaves_existing_config_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let base = Config {
            providers: crate::config::ProviderConfig {
                anthropic_api_key: Some("sk-ant-keep".to_string()),
                openai_api_key: None,
                google_api_key: None,
                anthropic_base_url: None,
                openai_base_url: None,
                google_base_url: None,
                openrouter_base_url: None,
                claude_code_enabled: false,
                claude_code_binary: None,
                claude_code_effort: None,
                anthropic_cache_ttl: None,
                fallback_order: Vec::new(),
                ..Default::default()
            },
            ..Default::default()
        };
        base.save_to_path_public(&path).unwrap();

        let resp = put_config_request(state_with_config_path(path.clone()), "{}").await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let saved = Config::load_from_path_public(&path).unwrap();
        assert_eq!(
            saved.providers.anthropic_api_key.as_deref(),
            Some("sk-ant-keep")
        );
    }

    /// A gateway can be created, then edited field by field, then removed,
    /// without the caller ever holding its key.
    ///
    /// This is the whole point of the partial update reaching gateways: a
    /// browser form that had to send the key back to change a URL would have
    /// to have been given the key, which `GET /api/config` deliberately never
    /// does.
    #[tokio::test]
    async fn put_config_edits_a_gateway_without_being_told_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_to_path_public(&path).unwrap();
        let state = || state_with_config_path(path.clone());

        // Create.
        let resp = put_config_request(
            state(),
            r#"{"gateways":[{"name":"groq","base_url":"https://api.groq.com","api_key":"sk-secret","script":"groq.rhai"}]}"#,
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let saved = Config::load_from_path_public(&path).unwrap();
        assert_eq!(
            saved.model_providers["groq"].script.as_deref(),
            Some("groq.rhai"),
            "the script backing the gateway is written too"
        );

        // Edit only the URL. The key is not sent, and must survive.
        let resp = put_config_request(
            state(),
            r#"{"gateways":[{"name":"groq","base_url":"https://eu.groq.com"}]}"#,
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let saved = Config::load_from_path_public(&path).unwrap();
        let gateway = &saved.model_providers["groq"];
        assert_eq!(gateway.base_url.as_deref(), Some("https://eu.groq.com"));
        assert_eq!(
            gateway.api_key.as_deref(),
            Some("sk-secret"),
            "an unsent key is left alone, not cleared"
        );

        // A second gateway leaves the first alone.
        let resp = put_config_request(state(), r#"{"gateways":[{"name":"other"}]}"#).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let saved = Config::load_from_path_public(&path).unwrap();
        assert_eq!(saved.model_providers.len(), 2);

        // Remove takes a list of its own, because omitting a gateway above
        // means "leave it alone" and so can never mean "delete it".
        let resp = put_config_request(state(), r#"{"remove_gateways":["other"]}"#).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let saved = Config::load_from_path_public(&path).unwrap();
        assert!(saved.model_providers.contains_key("groq"));
        assert!(!saved.model_providers.contains_key("other"));
    }

    /// An endpoint gateway round-trips its kind, headers and models, an
    /// unsent field is left alone, and the two refusals name the gateway.
    #[tokio::test]
    async fn put_config_writes_an_endpoint_gateway_and_refuses_a_broken_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_to_path_public(&path).unwrap();
        let state = || state_with_config_path(path.clone());

        let resp = put_config_request(
            state(),
            r#"{"gateways":[{"name":"llama","kind":"openai-compatible","base_url":"http://localhost:8080/v1","headers":{"X-Org":"r"},"models":["llama-3"]}]}"#,
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["gateways"][0]["kind"], "openai-compatible");
        assert_eq!(
            json["gateways"][0]["header_names"],
            serde_json::json!(["X-Org"])
        );
        assert_eq!(
            json["gateways"][0]["models"],
            serde_json::json!(["llama-3"])
        );
        assert!(!String::from_utf8_lossy(&body).contains("\"r\""), "{json}");
        let saved = Config::load_from_path_public(&path).unwrap();
        let entry = &saved.model_providers["llama"];
        assert!(entry.is_endpoint());
        assert_eq!(
            entry.header_pairs(),
            vec![("X-Org".to_string(), "r".to_string())]
        );

        // Edit only the URL: the headers and models survive.
        let resp = put_config_request(
            state(),
            r#"{"gateways":[{"name":"llama","base_url":"http://localhost:8081/v1"}]}"#,
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let saved = Config::load_from_path_public(&path).unwrap();
        let entry = &saved.model_providers["llama"];
        assert_eq!(entry.base_url.as_deref(), Some("http://localhost:8081/v1"));
        assert!(entry.headers.is_some());
        assert_eq!(entry.models.as_deref().map(|m| m.len()), Some(1));

        // An unknown kind is refused before anything is touched.
        let resp =
            put_config_request(state(), r#"{"gateways":[{"name":"llama","kind":"vllm"}]}"#).await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("unknown kind 'vllm'"));

        // An endpoint with no address is refused and not written.
        let resp = put_config_request(
            state(),
            r#"{"gateways":[{"name":"bare","kind":"openai-compatible"}]}"#,
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("[model_providers.bare]"));
        let saved = Config::load_from_path_public(&path).unwrap();
        assert!(!saved.model_providers.contains_key("bare"));

        // A script entry keeps reporting itself as one.
        let resp = put_config_request(
            state(),
            r#"{"gateways":[{"name":"groq","kind":"script","script":"groq.rhai"}]}"#,
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let saved = Config::load_from_path_public(&path).unwrap();
        assert!(!saved.model_providers["groq"].is_endpoint());
    }

    /// A refused write leaves the file byte for byte as it was.
    ///
    /// The test above proves the refused entry is absent, which a partial
    /// write could still satisfy. This is the stronger claim, and the one that
    /// matters: a request that answers 400 must not be able to leave the
    /// machine's config in a state nobody asked for.
    #[tokio::test]
    async fn a_refused_put_writes_nothing_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_to_path_public(&path).unwrap();
        let before = std::fs::read(&path).unwrap();

        // One request that both edits something valid and asks for something
        // refused: the valid half must not survive the refusal.
        let resp = put_config_request(
            state_with_config_path(path.clone()),
            r#"{"default_provider":"openai","gateways":[{"name":"bare","kind":"openai-compatible"}]}"#,
        )
        .await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "a refused write must not touch the file"
        );
    }

    async fn probe(body: serde_json::Value) -> (axum::http::StatusCode, serde_json::Value) {
        let app = Router::new().route("/api/models/probe", axum::routing::post(probe_models));
        let req = Request::builder()
            .method("POST")
            .uri("/api/models/probe")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// The probe lists what the server lists, sorted, sending the key and
    /// headers it was given; a refusal comes back as a 502 with the server's
    /// words, and a URL with no scheme never leaves the process.
    #[tokio::test]
    async fn the_probe_lists_a_servers_models_or_relays_its_refusal() {
        let listing = br#"{"data":[{"id":"zeta"},{"id":"alpha"}]}"#;
        let url = leviath_testkit::spawn_mock_server(200, "OK", listing).await;
        let (status, json) = probe(serde_json::json!({
            "base_url": url,
            "api_key": "k",
            "headers": {"X-Org": "r"},
        }))
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(json["models"], serde_json::json!(["alpha", "zeta"]));

        let url = leviath_testkit::spawn_mock_server(404, "Not Found", b"no such route").await;
        let (status, json) = probe(serde_json::json!({ "base_url": url, "api_key": " " })).await;
        assert_eq!(status, axum::http::StatusCode::BAD_GATEWAY);
        assert!(
            json["error"].as_str().unwrap().contains("no such route"),
            "{json}"
        );

        let (status, json) = probe(serde_json::json!({ "base_url": "localhost:8080" })).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(
            json["error"].as_str().unwrap().contains("http://"),
            "{json}"
        );
    }

    /// A machine that cannot build an HTTPS client cannot probe, and says so
    /// as a gateway failure rather than a panic.
    #[tokio::test]
    async fn the_probe_reports_a_client_that_will_not_build() {
        let failing: leviath_providers::provider::HttpClientFactory<'_> =
            &|_| Err(leviath_providers::provider::malformed_url_error());
        let result = probe_models_with(
            ProbeModelsReq {
                base_url: "http://127.0.0.1:1/v1".to_string(),
                api_key: None,
                headers: None,
            },
            failing,
        )
        .await;
        let (status, _) = result.expect_err("fails");
        assert_eq!(status, axum::http::StatusCode::BAD_GATEWAY);
    }

    /// The response a write returns reports the gateway the same redacted way
    /// a read does, so a form can render straight from it.
    #[tokio::test]
    async fn put_config_returns_the_gateway_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::default().save_to_path_public(&path).unwrap();

        let resp = put_config_request(
            state_with_config_path(path),
            r#"{"gateways":[{"name":"groq","api_key":"sk-secret"}]}"#,
        )
        .await;
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["gateways"][0]["name"], serde_json::json!("groq"));
        assert_eq!(json["gateways"][0]["has_api_key"], serde_json::json!(true));
        assert!(
            !String::from_utf8_lossy(&body).contains("sk-secret"),
            "the write's own response must not hand the key back"
        );
    }

    #[tokio::test]
    async fn put_config_read_failure_is_500() {
        // config_path points at a directory, so reading it as a file fails.
        let dir = tempfile::tempdir().unwrap();
        let resp = put_config_request(state_with_config_path(dir.path().to_path_buf()), "{}").await;
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn put_config_write_failure_is_500() {
        // The config file's parent is itself a file, so saving fails while
        // reading (a non-existent file) succeeds as defaults.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let path = blocker.join("config.toml");
        let resp = put_config_request(state_with_config_path(path), "{}").await;
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    // ─── config key validation ────────────────────────────────────────────────

    #[test]
    fn validate_key_format_covers_every_provider() {
        assert_eq!(validate_key_format("anthropic", "sk-ant-1"), (true, None));
        assert!(!validate_key_format("anthropic", "nope").0);
        assert_eq!(validate_key_format("openai", "sk-1"), (true, None));
        assert!(!validate_key_format("openai", "nope").0);
        assert_eq!(validate_key_format("google", "g"), (true, None));
        assert!(!validate_key_format("google", "  ").0);
        assert_eq!(validate_key_format("openrouter", "or"), (true, None));
        // A name this build does not know is a custom gateway, not a mistake.
        // Its key has no house format, so the only judgement available is
        // whether one was given at all.
        assert_eq!(validate_key_format("my-gateway", "anything"), (true, None));
        assert!(!validate_key_format("my-gateway", "   ").0);
    }

    // ─── custom gateways ──────────────────────────────────────────────────────

    /// The key never leaves the process, and neither does anything in `extra`.
    ///
    /// `extra` is forwarded into a provider script, so people keep credentials
    /// there. Reporting its names is what a form needs; reporting its values
    /// would be the same disclosure the `has_*_key` booleans exist to prevent.
    #[test]
    fn a_gateways_secrets_are_reported_as_presence_not_value() {
        let mut config = Config::default();
        config.model_providers.insert(
            "groq".to_string(),
            crate::config::ModelProviderConfig {
                script: Some("groq.rhai".to_string()),
                api_key: Some("sk-secret-value".to_string()),
                base_url: Some("https://api.groq.com".to_string()),
                rate_limit: None,
                serves: None,
                extra: [(
                    "signing_secret".to_string(),
                    toml::Value::String("hunter2".to_string()),
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        );

        config.model_providers.insert(
            "llama".to_string(),
            crate::config::ModelProviderConfig {
                kind: Some(crate::config::ModelProviderKind::OpenaiCompatible),
                base_url: Some("http://localhost:8080/v1".to_string()),
                headers: Some(
                    [("X-Api-Key".to_string(), "header-secret-value".to_string())]
                        .into_iter()
                        .collect(),
                ),
                models: Some(vec!["llama-3".to_string()]),
                ..Default::default()
            },
        );

        let redacted = redact(&config, &Default::default(), &healthy());
        let gateway = &redacted.gateways[0];
        assert_eq!(gateway.name, "groq");
        assert_eq!(gateway.kind, "script");
        assert_eq!(gateway.base_url.as_deref(), Some("https://api.groq.com"));
        assert!(gateway.has_api_key);
        assert_eq!(gateway.extra_keys, vec!["signing_secret".to_string()]);
        let endpoint = &redacted.gateways[1];
        assert_eq!(endpoint.kind, "openai-compatible");
        assert_eq!(endpoint.header_names, vec!["X-Api-Key".to_string()]);
        assert_eq!(endpoint.models, vec!["llama-3".to_string()]);
        assert!(!endpoint.has_api_key);

        // The whole serialized document, because a leak anywhere in it is a
        // leak: a field added later would otherwise carry the value silently.
        let json = serde_json::to_string(&redacted).expect("serializes");
        assert!(!json.contains("sk-secret-value"), "{json}");
        assert!(!json.contains("hunter2"), "{json}");
        assert!(!json.contains("header-secret-value"), "{json}");
    }

    /// Name-sorted, because the config holds gateways in a `HashMap` and a
    /// list that reorders itself between two reads cannot be edited in a form.
    #[test]
    fn gateways_are_reported_in_a_stable_order() {
        let mut config = Config::default();
        for name in ["zulu", "alpha", "mike"] {
            config
                .model_providers
                .insert(name.to_string(), Default::default());
        }
        let names: Vec<String> = gateways_of(&config).into_iter().map(|g| g.name).collect();
        assert_eq!(names, vec!["alpha", "mike", "zulu"]);
    }

    #[test]
    fn a_config_with_no_gateways_reports_none() {
        assert_eq!(gateways_of(&Config::default()), Vec::new());
    }

    /// A base URL is checked for shape only, and the scheme is the part people
    /// actually leave off.
    #[test]
    fn a_base_url_is_checked_for_its_scheme() {
        assert_eq!(validate_base_url("https://api.example.com"), (true, None));
        assert_eq!(validate_base_url("http://localhost:11434"), (true, None));
        assert!(!validate_base_url("api.example.com").0);
        assert!(!validate_base_url("  ").0);
    }

    #[tokio::test]
    async fn validate_config_key_endpoint_returns_result() {
        let app = Router::new().route(
            "/api/config/validate",
            axum::routing::post(validate_config_key),
        );
        let req = Request::builder()
            .method("POST")
            .uri("/api/config/validate")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"provider":"anthropic","key":"bad"}).to_string(),
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: ValidateKeyResp = serde_json::from_slice(&bytes).unwrap();
        assert!(!v.valid);
        assert!(v.message.is_some());
    }

    /// A gateway is checked on its URL as well as its key, and the URL is what
    /// answers when it is wrong.
    #[tokio::test]
    async fn validate_config_key_endpoint_checks_a_gateways_base_url() {
        let check = |body: serde_json::Value| async move {
            let app = Router::new().route(
                "/api/config/validate",
                axum::routing::post(validate_config_key),
            );
            let req = Request::builder()
                .method("POST")
                .uri("/api/config/validate")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice::<ValidateKeyResp>(&bytes).unwrap()
        };

        // A URL with no scheme is rejected, and the message names the URL
        // rather than the key, which is the part that is fine.
        let bad = check(serde_json::json!({
            "provider": "my-gateway",
            "key": "anything",
            "base_url": "api.example.com",
        }))
        .await;
        assert!(!bad.valid);
        assert!(
            bad.message.unwrap_or_default().contains("Base URL"),
            "the address is what is wrong"
        );

        // A good URL falls through to the key check, which for a custom
        // gateway only asks that a key was given at all.
        let good = check(serde_json::json!({
            "provider": "my-gateway",
            "key": "anything",
            "base_url": "https://api.example.com",
        }))
        .await;
        assert!(good.valid, "{:?}", good.message);

        // And an empty key still fails once the URL is fine.
        let empty = check(serde_json::json!({
            "provider": "my-gateway",
            "key": "  ",
            "base_url": "https://api.example.com",
        }))
        .await;
        assert!(!empty.valid);
    }

    #[tokio::test]
    async fn the_models_endpoint_is_empty_when_no_https_client_can_be_built() {
        // A keyed provider, so something actually asks for a client and the
        // failing factory has a request to fail. Ollama will not do: it
        // registers only when its address answers, so on a machine without it
        // nothing asks for a client, no error is produced, and this test
        // passes while exercising none of what it is named for.
        let (tx, _) = broadcast::channel::<ServerEvent>(64);
        let state = AppState {
            update_check: Default::default(),
            update_jobs: Default::default(),
            config: crate::commands::serve::testutil::fixed_config(Config {
                providers: crate::config::ProviderConfig {
                    anthropic_api_key: Some("test-key".to_string()),
                    ..Config::default().providers
                },
                ..Config::default()
            }),
            event_tx: tx,
            control: crate::commands::serve::testutil::no_daemon_client(),
            mcp: crate::commands::serve::mcp::McpAdmin::default(),
            providers: crate::commands::serve::providers::ProviderAdmin::default(),
            limits: Default::default(),
        };
        let Json(models) = super::models_with(
            &state,
            &|_t| Err(leviath_providers::provider::malformed_url_error()),
            &Default::default(),
        )
        .await;
        assert!(models.is_empty());
    }
}
