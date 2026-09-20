//! Writing the machine's config.
//!
//! One place applies a partial edit, so the two surfaces cannot disagree about
//! what a field means. Three states run through it: a field left out leaves the
//! setting alone, `null` clears it, and a value sets it. An empty string is
//! refused rather than read as a clear, because a form that posts its empty box
//! should be told rather than obeyed.
//!
//! Every refusal happens before anything is written, so a request that is going
//! to fail leaves the file exactly as it was.

use crate::config::Config;

use super::super::config_types::WriteConfigReq;
use super::error::ServeError;

/// The reasoning efforts the Codex provider accepts.
const CODEX_EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh"];

/// The text verbosities it accepts.
const CODEX_VERBOSITIES: &[&str] = &["low", "medium", "high"];

/// Check a value against the words the provider understands.
///
/// The provider silently ignores a setting it does not know, so a typo saved
/// here would read as the feature not working.
fn validated(
    value: Option<String>,
    allowed: &[&str],
    what: &str,
) -> Result<Option<String>, ServeError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if allowed.contains(&value.as_str()) {
        return Ok(Some(value));
    }
    Err(ServeError::BadRequest(format!(
        "unknown Codex {what} '{value}'; use one of {}",
        allowed.join(", ")
    )))
}

/// Apply a partial edit to the config on disk, and answer with what it became.
pub(crate) fn write(req: WriteConfigReq) -> Result<Config, ServeError> {
    let paths = super::super::mcp::admin_paths();
    let path = &paths.config;
    let mut config = Config::load_from_path_public(path)
        .map_err(|e| ServeError::Internal(format!("failed to read config: {e}")))?;

    if let Some(v) = req.default_provider {
        config.default_provider = v;
    }
    // A present list replaces the order whole; an empty one clears it back to
    // `default_provider` alone. Absent leaves it untouched, like every other
    // partial field here.
    if let Some(order) = req.provider_order {
        config.providers.provider_order = order;
    }
    // Three states rather than the two every field around it has: absent
    // leaves the pin alone, `null` removes it so each blueprint picks its own
    // model again, and a string pins that one. Written as a `match` because
    // the read has to distinguish "the key was not sent" from "the key was
    // sent as null", which an `if let Some` on a single `Option` cannot.
    // Refused rather than treated as a clear: `""` is not a model id, and a
    // form that posts its empty box should be told, not obeyed. The check runs
    // before anything is saved, so the file is untouched.
    for (key, sent, slot) in [
        (
            "override_model",
            req.override_model,
            &mut config.override_model,
        ),
        (
            "fallback_model",
            req.fallback_model,
            &mut config.fallback_model,
        ),
    ] {
        match sent {
            None => {}
            Some(None) => *slot = None,
            Some(Some(v)) if v.trim().is_empty() => {
                return Err(ServeError::BadRequest(format!(
                    "{key} must not be empty; send null to clear it"
                )));
            }
            Some(Some(v)) => *slot = Some(v),
        }
    }
    // The keys have the same three states: `null` clears one, which takes
    // the provider out of this install the way the setup wizard's remove
    // does, and an empty string is refused rather than stored as a key
    // that authenticates as nobody.
    for (key, sent, slot) in [
        (
            "anthropic_key",
            req.anthropic_key,
            &mut config.providers.anthropic_api_key,
        ),
        (
            "openai_key",
            req.openai_key,
            &mut config.providers.openai_api_key,
        ),
        (
            "google_key",
            req.google_key,
            &mut config.providers.google_api_key,
        ),
        (
            "openrouter_key",
            req.openrouter_key,
            &mut config.openrouter_api_key,
        ),
        (
            "bedrock_key",
            req.bedrock_key,
            &mut config.providers.bedrock_api_key,
        ),
        ("xai_key", req.xai_key, &mut config.providers.xai_api_key),
        ("meta_key", req.meta_key, &mut config.providers.meta_api_key),
    ] {
        match sent {
            None => {}
            Some(None) => *slot = None,
            Some(Some(v)) if v.trim().is_empty() => {
                return Err(ServeError::BadRequest(format!(
                    "{key} must not be empty; send null to clear it"
                )));
            }
            Some(Some(v)) => *slot = Some(v),
        }
    }
    if let Some(v) = req.bedrock_region {
        if v.trim().is_empty() {
            return Err(ServeError::BadRequest(
                "bedrock_region must not be empty".to_string(),
            ));
        }
        config.providers.bedrock_region = Some(v.trim().to_string());
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
    config.providers.grok_enabled = req.grok_enabled.unwrap_or(config.providers.grok_enabled);
    config.providers.file_uploads = req.file_uploads.unwrap_or(config.providers.file_uploads);
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
                    ServeError::BadRequest(format!(
                        "gateway '{}': unknown kind '{text}'; use \"script\", \
                             \"openai-compatible\" or \"openai\"",
                        gateway.name
                    ))
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
            .map_err(|e| ServeError::BadRequest(e.to_string()))?;
    }

    config
        .save_to_path_public(path)
        .map_err(|e| ServeError::Internal(format!("failed to write config: {e}")))?;

    Ok(config)
}
