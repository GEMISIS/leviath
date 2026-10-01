//! The `[stages.<name>.model]` table: which models a stage may run on, and
//! the parameters it hands them.
//!
//! Split from `stage.rs` by concern: the stage parser reads a stage's shape,
//! this reads the one sub-table that names a model.

use super::*;

/// Parse `[stages.<name>.model]`, or the shipped default when the stage does
/// not name one.
pub(super) fn parse_stage_model(
    stage_name: &str,
    stage_value: &toml::Value,
) -> Result<ModelConfig> {
    let model_table = table_of(stage_value, "model");
    if let Some(mt) = model_table {
        let mut models = Vec::new();

        // An entry may be a bare model name, or a table naming a provider only
        // when the route matters (a local or self-hosted model). An absent
        // provider is EMPTY, not "anthropic": an author cannot know what a
        // machine has configured, and defaulting made omission a silent choice.
        if let Some(models_arr) = array_of(mt, "models") {
            for entry in models_arr {
                if let Some(name) = entry.as_str() {
                    models.push(ModelEntry::new(String::new(), name.to_string()));
                    continue;
                }
                // A table naming no model names nothing, so it is dropped.
                if let Some(t) = entry.as_table()
                    && let Some(model) = str_of(t, "model")
                {
                    let route = str_of(t, "provider");
                    let route = route.unwrap_or_default().to_string();
                    models.push(ModelEntry::new(route, model.to_string()));
                }
            }
        }

        // Backward compat: old single-model format (provider + model at
        // top level) or old fallbacks list - treat both as models entries.
        if models.is_empty() {
            if let Some(provider) = str_of(mt, "provider") {
                let model_name = str_of(mt, "model").unwrap_or("claude-sonnet-4-6");
                models.push(ModelEntry::new(
                    provider.to_string(),
                    model_name.to_string(),
                ));
            }

            // Old fallbacks become additional models entries
            if let Some(fallbacks_arr) = array_of(mt, "fallbacks") {
                for fb in fallbacks_arr {
                    if let Some(fb_table) = fb.as_table() {
                        models.push(ModelEntry::new(
                            str_of(fb_table, "provider")
                                .unwrap_or("anthropic")
                                .to_string(),
                            str_of(fb_table, "model")
                                .unwrap_or("claude-sonnet-4-6")
                                .to_string(),
                        ));
                    }
                }
            }
        }

        // If still empty, use defaults
        if models.is_empty() {
            models.push(ModelEntry::new(
                "anthropic".to_string(),
                "claude-sonnet-4-6".to_string(),
            ));
        }

        let allow_user_default = bool_of(mt, "allow_user_default").unwrap_or(true);

        // Parse parameters
        let mut parameters = std::collections::HashMap::new();
        if let Some(params) = table_of(mt, "parameters") {
            for (k, v) in params {
                // Converting a parsed `toml::Value` to JSON is infallible:
                // serde_json maps non-finite floats to null rather than
                // erroring, and every other toml scalar/collection maps
                // cleanly.
                let json_val = serde_json::to_value(v)
                    .expect("infallible: toml::Value always converts to serde_json::Value");
                parameters.insert(k.clone(), json_val);
            }
        }

        let request_timeout_secs = count_of(
            mt,
            &format!("stage '{stage_name}': model"),
            "request_timeout_secs",
        )?
        .map(|secs| secs as u64);

        Ok(ModelConfig {
            models,
            allow_user_default,
            parameters,
            request_timeout_secs,
        })
    } else {
        Ok(ModelConfig::new(
            "anthropic".to_string(),
            "claude-sonnet-4-6".to_string(),
        ))
    }
}

/// Every key [`parse_stage_model`] reads off a `[stages.<name>.model]`
/// table, for the schema guard in `tests.rs`. Like `REGION_KEYS`, a list and
/// not a check: the parser ignores what it does not know.
#[cfg(test)]
pub(super) const MODEL_KEYS: &[&str] = &[
    "allow_user_default",
    "fallbacks",
    "model",
    "models",
    "parameters",
    "provider",
    "request_timeout_secs",
];
