//! Stage model and tool resolution: turning a blueprint's per-stage
//! [`ModelConfig`] and `available_tools` into concrete [`ResolvedStage`]s
//! against whatever providers and tools the host actually has.
//!
//! Lives in the runtime (rather than the CLI daemon, where it started) so an
//! embedding host resolves stages exactly the way `lev run` does. The one
//! policy input the CLI used to read from its config file - the user's default
//! provider/model - arrives as a plain [`ModelDefaults`] value instead.

use leviath_core::Blueprint;
use leviath_core::blueprint::ModelConfig;

use super::ResolvedStage;
use crate::providers::ProviderRegistry;
use leviath_providers::Tool;

/// The user's default provider/model, the fallback when none of a stage's
/// listed models has a registered provider. The CLI fills this from
/// `config.toml`; an embedder sets it on the world builder (or leaves it
/// empty, keeping the blueprint's own entries as the last resort).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelDefaults {
    /// The default provider name (e.g. `anthropic`).
    pub provider: String,
    /// The default model, if the user configured one.
    pub model: Option<String>,
}

/// Resolve a stage's [`ModelConfig`] to a concrete `(provider, model)` against
/// the registered providers. Honors a `--model` override (`provider/model` or a
/// bare `model`), otherwise picks the first listed model whose provider is
/// registered, then falls back to the user default (when `allow_user_default`),
/// and finally to the config's first listed entry. (Ported from the executor's
/// inline resolution.)
pub fn resolve_stage_model(
    model_cfg: &ModelConfig,
    model_override: Option<&str>,
    defaults: &ModelDefaults,
    registry: &ProviderRegistry,
) -> (String, String) {
    let (override_provider, override_model) = match model_override {
        Some(ov) if ov.contains('/') => {
            let (p, m) = ov.split_once('/').unwrap();
            (Some(p.to_string()), Some(m.to_string()))
        }
        Some(ov) => (None, Some(ov.to_string())),
        None => (None, None),
    };

    // Full provider/model override wins outright.
    if let Some(provider) = override_provider {
        return (provider, override_model.unwrap_or_default());
    }

    // First listed model whose provider is registered.
    for entry in &model_cfg.models {
        if registry.has(&entry.provider) {
            let model = override_model
                .clone()
                .unwrap_or_else(|| entry.model.clone());
            return (entry.provider.clone(), model);
        }
    }

    // Fall back to the user's default model, or finally the first listed entry.
    user_default_model(model_cfg, override_model.as_deref(), defaults, registry).unwrap_or_else(
        || {
            (
                model_cfg.provider().to_string(),
                model_cfg.model().to_string(),
            )
        },
    )
}

/// The user-default fallback for [`resolve_stage_model`]: `None` when the stage
/// forbids it or no usable default exists.
fn user_default_model(
    model_cfg: &ModelConfig,
    override_model: Option<&str>,
    defaults: &ModelDefaults,
    registry: &ProviderRegistry,
) -> Option<(String, String)> {
    if !model_cfg.allow_user_default {
        return None;
    }
    if let Some(model) = override_model {
        return Some((defaults.provider.clone(), model.to_string()));
    }
    if let Some(default_model) = &defaults.model
        && registry.has(&defaults.provider)
    {
        return Some((defaults.provider.clone(), default_model.clone()));
    }
    None
}

/// Filter `all` tool defs down to those a stage's `available_tools` names
/// (alias-resolved). Shared by spawn-time stage resolution and the mid-run
/// tool-service refresh so both apply Layer-1 identically.
pub fn filter_tools_by_available(all: &[Tool], available: &[String]) -> Vec<Tool> {
    if available.is_empty() {
        return Vec::new();
    }
    all.iter()
        .filter(|t| {
            available
                .iter()
                .any(|n| leviath_tools::canonical_tool_name(n) == t.name)
        })
        .cloned()
        .collect()
}

/// Every provider a stage could have used, in the order they were tried, for
/// the error message when none of them is configured.
///
/// A `--model provider/model` override is the whole list on its own: it names
/// exactly one provider and skips the blueprint's fallbacks entirely.
///
/// Public because [`resolve_stages`] is not the only place that has to explain
/// an unusable resolution: `lev doctor` runs the same chain against an empty
/// [`ModelConfig`] to report what the user's config alone would pick, and it
/// must name the same providers in the same order rather than reimplement this.
pub fn providers_tried(
    model_cfg: &ModelConfig,
    model_override: Option<&str>,
    defaults: &ModelDefaults,
) -> String {
    let mut names: Vec<String> = match model_override {
        Some(ov) if ov.contains('/') => vec![
            ov.split_once('/')
                .map(|(p, _)| p.to_string())
                .expect("the `contains('/')` guard guarantees a split"),
        ],
        _ => {
            let mut listed: Vec<String> = model_cfg
                .models
                .iter()
                .map(|e| e.provider.clone())
                .collect();
            if model_cfg.allow_user_default && !defaults.provider.is_empty() {
                listed.push(defaults.provider.clone());
            }
            listed
        }
    };
    names.dedup();
    names.join(", ")
}

/// Resolve every stage's provider/model + effective tool set from the
/// blueprint, or report the first stage that has no usable provider.
///
/// The last fallback in [`resolve_stage_model`] is unchecked - it hands back
/// the blueprint's own first entry whether or not anything answers to that
/// name, and a full `provider/model` override skips the registry outright. So
/// a stage could resolve to a provider that does not exist, and the agent
/// spawned anyway: `Active`, iteration 0, and unable to take a single turn for
/// as long as the host lived (issue #190). Catching it here turns a silently
/// wedged run into an error the caller sees.
pub fn resolve_stages(
    blueprint: &Blueprint,
    model_override: Option<&str>,
    defaults: &ModelDefaults,
    registry: &ProviderRegistry,
    all_tool_defs: &[Tool],
) -> Result<Vec<ResolvedStage>, String> {
    blueprint
        .stages
        .iter()
        .map(|stage| {
            let (provider_name, model) =
                resolve_stage_model(&stage.model, model_override, defaults, registry);
            // `registry.has` also consults the script layer, so a `.rhai`
            // provider sitting on disk counts as usable and is never
            // false-rejected here.
            if !registry.has(&provider_name) {
                return Err(format!(
                    "stage '{}' has no usable provider (tried: {}). Configure one \
                     with `lev setup`, or add it to config.toml and restart the daemon.",
                    stage.name,
                    providers_tried(&stage.model, model_override, defaults)
                ));
            }
            // Empty `available_tools` exposes no tools; otherwise filter the full
            // set by name (alias-resolved). A name matching nothing (a typo, or an
            // MCP tool whose server isn't installed) is simply omitted.
            let tools = filter_tools_by_available(all_tool_defs, &stage.available_tools);
            Ok(ResolvedStage {
                provider_name,
                model,
                tools,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_core::blueprint::ModelEntry;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn model_cfg(models: Vec<(&str, &str)>) -> ModelConfig {
        ModelConfig {
            models: models
                .into_iter()
                .map(|(p, m)| ModelEntry {
                    provider: p.to_string(),
                    model: m.to_string(),
                })
                .collect(),
            allow_user_default: true,
            parameters: HashMap::new(),
            request_timeout_secs: None,
        }
    }

    fn registry_with(providers: &[&str]) -> ProviderRegistry {
        let mut r = ProviderRegistry::new();
        for p in providers {
            r.register(p.to_string(), Arc::new(FakeProvider));
        }
        r
    }

    struct FakeProvider;
    #[async_trait::async_trait]
    impl leviath_providers::Provider for FakeProvider {
        async fn infer(
            &self,
            _r: leviath_providers::InferenceRequest,
        ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
            Err(leviath_providers::ProviderError::Other(
                "test provider".to_string(),
            ))
        }
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            1000
        }
        fn name(&self) -> &str {
            "fake"
        }
        fn capabilities(&self, _m: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
    }

    #[tokio::test]
    async fn fake_provider_is_a_minimal_registry_stub() {
        // The resolver only asks the registry `has()`, so the fixture provider
        // is inert; this pins its stub answers so the impl stays measured.
        use leviath_providers::Provider as _;
        let p = FakeProvider;
        let request: leviath_providers::InferenceRequest =
            serde_json::from_value(serde_json::json!({
                "messages": [],
                "model": "m",
                "max_tokens": 1,
                "temperature": 0.0,
                "tools": [],
                "extra": null,
            }))
            .unwrap();
        assert!(p.infer(request).await.is_err());
        assert_eq!(p.count_tokens("x", "m").await, 1);
        assert_eq!(p.max_context_tokens("m"), 1000);
        assert_eq!(p.name(), "fake");
        let _ = p.capabilities("m");
    }

    #[test]
    fn resolve_full_override_wins() {
        let (p, m) = resolve_stage_model(
            &model_cfg(vec![("anthropic", "x")]),
            Some("openai/gpt-5"),
            &ModelDefaults::default(),
            &registry_with(&[]),
        );
        assert_eq!((p.as_str(), m.as_str()), ("openai", "gpt-5"));
    }

    #[test]
    fn resolve_first_available_model() {
        // anthropic not registered, openai is → picks openai.
        let (p, m) = resolve_stage_model(
            &model_cfg(vec![("anthropic", "a"), ("openai", "o")]),
            None,
            &ModelDefaults::default(),
            &registry_with(&["openai"]),
        );
        assert_eq!((p.as_str(), m.as_str()), ("openai", "o"));
    }

    #[test]
    fn resolve_model_only_override_keeps_available_provider() {
        let (p, m) = resolve_stage_model(
            &model_cfg(vec![("openai", "o")]),
            Some("gpt-override"),
            &ModelDefaults::default(),
            &registry_with(&["openai"]),
        );
        assert_eq!((p.as_str(), m.as_str()), ("openai", "gpt-override"));
    }

    #[test]
    fn resolve_user_default_when_nothing_listed_available() {
        // Listed provider "ghost" is unavailable; anthropic (the default) is.
        let defaults = ModelDefaults {
            provider: "anthropic".to_string(),
            model: Some("claude-default".to_string()),
        };
        let (p, m) = resolve_stage_model(
            &model_cfg(vec![("ghost", "g")]),
            None,
            &defaults,
            &registry_with(&["anthropic"]),
        );
        assert_eq!((p.as_str(), m.as_str()), ("anthropic", "claude-default"));
    }

    #[test]
    fn resolve_user_default_with_model_override() {
        let defaults = ModelDefaults {
            provider: "anthropic".to_string(),
            model: None,
        };
        let (p, m) = resolve_stage_model(
            &model_cfg(vec![("ghost", "g")]),
            Some("just-a-model"),
            &defaults,
            &registry_with(&[]),
        );
        assert_eq!((p.as_str(), m.as_str()), ("anthropic", "just-a-model"));
    }

    #[test]
    fn resolve_user_default_provider_unavailable_falls_through() {
        // allow_user_default, a default model set, but the default provider isn't
        // registered ⇒ neither user-default branch fires ⇒ last resort.
        let defaults = ModelDefaults {
            provider: "ghost-default".to_string(),
            model: Some("dm".to_string()),
        };
        let (p, m) = resolve_stage_model(
            &model_cfg(vec![("ghost", "g")]),
            None,
            &defaults,
            &registry_with(&[]),
        );
        assert_eq!((p.as_str(), m.as_str()), ("ghost", "g"));
    }

    #[test]
    fn resolve_last_resort_first_listed() {
        // No override, nothing available, no usable default → first listed entry.
        let (p, m) = resolve_stage_model(
            &model_cfg(vec![("ghost", "g")]),
            None,
            &ModelDefaults::default(),
            &registry_with(&[]),
        );
        assert_eq!((p.as_str(), m.as_str()), ("ghost", "g"));
    }

    #[test]
    fn resolve_no_user_default_uses_last_resort() {
        let mut cfg = model_cfg(vec![("ghost", "g")]);
        cfg.allow_user_default = false; // forbid the default fallback
        let defaults = ModelDefaults {
            provider: "anthropic".to_string(),
            model: Some("would-be-default".to_string()),
        };
        let (p, m) = resolve_stage_model(&cfg, None, &defaults, &registry_with(&["anthropic"]));
        assert_eq!((p.as_str(), m.as_str()), ("ghost", "g"));
    }

    #[test]
    fn resolve_stages_empty_available_tools_gets_none() {
        let mut stage =
            leviath_core::Stage::new("s".to_string(), model_cfg(vec![("anthropic", "m")]));
        stage.available_tools = vec![]; // empty ⇒ no tools
        let layout = leviath_core::layout::ContextLayout::new(vec![], 1000);
        let bp = Blueprint::new("t".to_string(), "d".to_string(), vec![stage], layout);
        let tools = vec![Tool {
            name: "read_file".to_string(),
            description: String::new(),
            parameters: serde_json::Value::Null,
        }];
        let resolved = resolve_stages(
            &bp,
            None,
            &ModelDefaults::default(),
            &registry_with(&["anthropic"]),
            &tools,
        )
        .expect("anthropic is registered");
        assert!(resolved[0].tools.is_empty());
    }

    #[test]
    fn resolve_stages_refuses_a_stage_with_no_usable_provider() {
        // Issue #190: the last fallback in `resolve_stage_model` is unchecked,
        // so this used to resolve to "ghost" and produce an agent that could
        // never take a turn. It has to be an error the caller sees.
        let stage = leviath_core::Stage::new("plan".to_string(), model_cfg(vec![("ghost", "m")]));
        let layout = leviath_core::layout::ContextLayout::new(vec![], 1000);
        let bp = Blueprint::new("t".to_string(), "d".to_string(), vec![stage], layout);

        let err = resolve_stages(
            &bp,
            None,
            &ModelDefaults::default(),
            &registry_with(&[]),
            &[],
        )
        .expect_err("no provider is configured");

        assert!(err.contains("plan"), "names the stage: {err}");
        assert!(err.contains("ghost"), "names what it tried: {err}");
        assert!(err.contains("lev setup"), "says what to do: {err}");
    }

    #[test]
    fn resolve_stages_refuses_an_override_naming_an_unregistered_provider() {
        // `--model ghost/x` short-circuits every fallback, so the override is
        // the only provider that was tried.
        let stage =
            leviath_core::Stage::new("plan".to_string(), model_cfg(vec![("anthropic", "m")]));
        let layout = leviath_core::layout::ContextLayout::new(vec![], 1000);
        let bp = Blueprint::new("t".to_string(), "d".to_string(), vec![stage], layout);

        let err = resolve_stages(
            &bp,
            Some("ghost/x"),
            &ModelDefaults::default(),
            &registry_with(&["anthropic"]),
            &[],
        )
        .expect_err("the override names a provider that isn't registered");

        assert!(err.contains("tried: ghost"), "got: {err}");
        assert!(
            !err.contains("anthropic"),
            "the override skipped the blueprint's list entirely: {err}"
        );
    }

    #[test]
    fn providers_tried_lists_the_blueprint_entries_and_the_user_default() {
        let defaults = ModelDefaults {
            provider: "fallback".to_string(),
            model: None,
        };
        let cfg = model_cfg(vec![("one", "m"), ("two", "m")]);
        assert_eq!(providers_tried(&cfg, None, &defaults), "one, two, fallback");

        // A stage that opts out of the user default doesn't claim to have tried it.
        let mut no_default = cfg.clone();
        no_default.allow_user_default = false;
        assert_eq!(providers_tried(&no_default, None, &defaults), "one, two");

        // Neither does an embedder that configured no default at all.
        assert_eq!(
            providers_tried(&cfg, None, &ModelDefaults::default()),
            "one, two"
        );

        // A bare `--model m` override still uses the blueprint's providers.
        assert_eq!(
            providers_tried(&cfg, Some("m"), &defaults),
            "one, two, fallback"
        );
    }

    #[test]
    fn resolve_stages_matches_by_alias_and_skips_unknown_names() {
        // A stage names `bash` (an alias) and a not-installed MCP tool. The
        // filter must select the canonical `shell` definition for the alias and
        // silently omit the unknown name (no error, no panic).
        let mut stage =
            leviath_core::Stage::new("s".to_string(), model_cfg(vec![("anthropic", "m")]));
        stage.available_tools = vec!["bash".to_string(), "acme__uninstalled".to_string()];
        let layout = leviath_core::layout::ContextLayout::new(vec![], 1000);
        let bp = Blueprint::new("t".to_string(), "d".to_string(), vec![stage], layout);
        let tools = vec![
            Tool {
                name: "shell".to_string(),
                description: String::new(),
                parameters: serde_json::Value::Null,
            },
            Tool {
                name: "read_file".to_string(),
                description: String::new(),
                parameters: serde_json::Value::Null,
            },
        ];
        let resolved = resolve_stages(
            &bp,
            None,
            &ModelDefaults::default(),
            &registry_with(&["anthropic"]),
            &tools,
        )
        .expect("anthropic is registered");
        let selected: Vec<&str> = resolved[0].tools.iter().map(|t| t.name.as_str()).collect();
        // `bash` resolved to `shell`; the unknown MCP name and unlisted
        // `read_file` were both excluded.
        assert_eq!(selected, vec!["shell"]);
    }
}
