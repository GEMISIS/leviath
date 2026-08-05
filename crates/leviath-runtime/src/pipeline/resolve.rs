//! Stage model and tool resolution: turning a blueprint's per-stage
//! [`ModelConfig`] and `available_tools` into concrete [`ResolvedStage`]s
//! against whatever providers and tools the host actually has.
//!
//! Lives in the runtime (rather than the CLI daemon, where it started) so an
//! embedding host resolves stages exactly the way `lev run` does. The one
//! policy input the CLI used to read from its config file - the user's default
//! provider/model - arrives as a plain [`ModelDefaults`] value instead.

use leviath_core::Blueprint;
use leviath_core::blueprint::{ModelConfig, ModelEntry};

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
    /// The host-wide failover chain, from `[providers] fallback_order`.
    ///
    /// Appended after a stage's own entries and the user default, so a
    /// blueprint that names exactly one model still has somewhere to go when
    /// that provider stops answering. This is the case issue #201 reported:
    /// every stage named a single OpenRouter model, so there was nothing to
    /// fall back to when the account ran out of credits.
    pub fallback_order: Vec<ModelEntry>,
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
    let first = resolve_stage_candidates(model_cfg, model_override, defaults, registry)
        .into_iter()
        .next()
        .expect("resolve_stage_candidates always yields at least one entry");
    (first.provider, first.model)
}

/// Every provider/model this stage may run on, best first.
///
/// [`resolve_stage_model`] is this list's head. The tail is what the runtime
/// fails over to when a provider turns out to be unusable mid-run: the ordered
/// list in `ModelConfig.models` was only ever consulted for *registration* at
/// spawn time, so a provider that was configured but out of credits was picked
/// and then never abandoned (issue #201).
///
/// Order: the stage's own registered entries, then the user default, then the
/// host-wide `fallback_order`. Deduplicated, because the same pair reaching the
/// list twice would spend a failover step going nowhere. Never empty: with
/// nothing registered it yields the blueprint's own first entry, exactly as
/// before, and `resolve_stages` rejects that unusable case with a clear error.
pub fn resolve_stage_candidates(
    model_cfg: &ModelConfig,
    model_override: Option<&str>,
    defaults: &ModelDefaults,
    registry: &ProviderRegistry,
) -> Vec<ModelEntry> {
    let (override_provider, override_model) = match model_override {
        Some(ov) if ov.contains('/') => {
            let (p, m) = ov
                .split_once('/')
                .expect("the `contains('/')` guard splits");
            (Some(p.to_string()), Some(m.to_string()))
        }
        Some(ov) => (None, Some(ov.to_string())),
        None => (None, None),
    };

    // A full provider/model override names exactly one pair and deliberately
    // skips every fallback: the caller asked for that model, not a substitute.
    if let Some(provider) = override_provider {
        return vec![ModelEntry::new(
            provider,
            override_model.unwrap_or_default(),
        )];
    }

    let mut candidates: Vec<ModelEntry> = Vec::new();
    let mut push = |provider: String, model: String| {
        let entry = ModelEntry::new(provider, model);
        if !candidates
            .iter()
            .any(|c| c.provider == entry.provider && c.model == entry.model)
        {
            candidates.push(entry);
        }
    };

    // Every listed model whose provider is registered, in blueprint order. A
    // bare `--model` override renames the model but keeps the provider order.
    for entry in &model_cfg.models {
        if registry.has(&entry.provider) {
            let model = override_model
                .clone()
                .unwrap_or_else(|| entry.model.clone());
            push(entry.provider.clone(), model);
        }
    }

    if let Some((provider, model)) =
        user_default_model(model_cfg, override_model.as_deref(), defaults, registry)
    {
        push(provider, model);
    }

    // The host-wide chain last: it is the safety net for a blueprint that names
    // one model, not a preference over what the blueprint asked for.
    for entry in &defaults.fallback_order {
        if registry.has(&entry.provider) {
            push(entry.provider.clone(), entry.model.clone());
        }
    }

    // `default_provider = "openrouter"` is the user saying where their runs
    // should go. It was only ever consulted after every registered entry the
    // blueprint listed, so on a machine with an OpenRouter key it never won
    // anything: the bundled blueprints all name anthropic, openai and ollama,
    // and ollama registers with no key at all, so an OpenRouter-only install
    // dispatched every stage at a localhost server that was not running.
    //
    // Registered candidates on the user's default provider therefore move to
    // the front, keeping their relative order. A blueprint that must pin its
    // own provider already has the way to say so - `allow_user_default =
    // false` - and that suppresses this too.
    if model_cfg.allow_user_default && registry.has(&defaults.provider) {
        let (preferred, rest): (Vec<ModelEntry>, Vec<ModelEntry>) = candidates
            .into_iter()
            .partition(|c| c.provider == defaults.provider);
        candidates = preferred.into_iter().chain(rest).collect();
    }

    if candidates.is_empty() {
        // Nothing registered. Hand back the blueprint's own first entry so the
        // caller reports "no usable provider" against a name the user wrote,
        // rather than an empty list.
        candidates.push(ModelEntry::new(
            model_cfg.provider().to_string(),
            model_cfg.model().to_string(),
        ));
    }

    // The head keeps whatever `resolve_stage_model` has always produced, up to
    // and including an unregistered provider that `resolve_stages` then
    // rejects with a readable error. The *tail* is different: every entry in
    // it is somewhere the runtime will actually dispatch to, so an
    // unregistered one is not a fallback but a phantom that parks the run on
    // `StallReason::ProviderMissing`. `user_default_model` hands one back
    // whenever a bare `--model` override is in play, so filter here.
    let tail: Vec<ModelEntry> = candidates
        .split_off(1)
        .into_iter()
        .filter(|e| registry.has(&e.provider))
        .collect();
    candidates.extend(tail);
    candidates
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

/// The stage's Layer-1 tool set for a run that may have nobody watching.
///
/// Same filter as [`filter_tools_by_available`], then - for an unattended run -
/// minus every tool whose only outcome is a prompt for a person
/// ([`BLOCKING_INTERACTION_TOOLS`](crate::dynamic_interaction::BLOCKING_INTERACTION_TOOLS)),
/// unless the stage named it in `required_tools`.
///
/// Dropping the definition rather than auto-answering the call is what makes the
/// difference visible to the model: it never sees the tool, so it decides for
/// itself instead of spending a round trip to be told nobody is there. A call
/// that arrives anyway (a model repeating itself from context) meets the ordinary
/// unoffered-tool refusal.
pub fn filter_tools_for_stage(
    all: &[Tool],
    available: &[String],
    required: &[String],
    unattended: bool,
) -> Vec<Tool> {
    let mut tools = filter_tools_by_available(all, available);
    if unattended {
        tools.retain(|t| {
            !crate::dynamic_interaction::BLOCKING_INTERACTION_TOOLS.contains(&t.name.as_str())
                || required
                    .iter()
                    .any(|n| leviath_tools::canonical_tool_name(n) == t.name)
        });
    }
    tools
}

/// Rewrite the `submit_output` definition in `tools` to describe the shape this
/// stage is meant to produce.
///
/// This is the entire mechanism for arbitrary output formats. There is no
/// per-format code path anywhere: what makes a model emit a2ui, a house schema,
/// or something invented after this was written is that the format label, the
/// author's instructions, and a literal example are pasted into the description
/// the model reads. A stage that declares nothing keeps the generic wording.
///
/// A no-op when the stage does not offer the tool, which is most stages.
fn apply_output_shape(tools: &mut [Tool], spec: Option<&leviath_core::output::OutputSpec>) {
    let Some(spec) = spec else { return };
    let described = leviath_core::describe_spec(spec);
    if described.is_empty() {
        return;
    }
    for tool in tools
        .iter_mut()
        .filter(|t| t.name == leviath_tools::SUBMIT_OUTPUT_TOOL)
    {
        tool.description = leviath_tools::submit_output_description(&described);
    }
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
///
/// `unattended` is the run's `--yolo` setting: it decides whether a stage's
/// human-in-the-loop tools are advertised at all (see
/// [`filter_tools_for_stage`]).
///
/// `output_request` is the shape whoever launched the run asked for, if any. It
/// is resolved here, alongside the model and tool choices, because this is the
/// one place that can see all three levels at once - and because a caller's
/// request only exists at launch.
pub fn resolve_stages(
    blueprint: &Blueprint,
    model_override: Option<&str>,
    defaults: &ModelDefaults,
    registry: &ProviderRegistry,
    all_tool_defs: &[Tool],
    unattended: bool,
    output_request: Option<&leviath_core::output::OutputSpec>,
) -> Result<Vec<ResolvedStage>, String> {
    blueprint
        .stages
        .iter()
        .map(|stage| {
            let mut candidates =
                resolve_stage_candidates(&stage.model, model_override, defaults, registry);
            let head = candidates.remove(0);
            // `registry.has` also consults the script layer, so a `.rhai`
            // provider sitting on disk counts as usable and is never
            // false-rejected here.
            if !registry.has(&head.provider) {
                return Err(format!(
                    "stage '{}' has no usable provider (tried: {}). Configure one \
                     with `lev setup`, or add it to config.toml and restart the daemon.",
                    stage.name,
                    providers_tried(&stage.model, model_override, defaults)
                ));
            }
            // Empty `available_tools` exposes no tools; otherwise filter the full
            // set by name (alias-resolved). A name matching nothing (a typo, or an
            // MCP tool whose server isn't installed) is simply omitted. An
            // unattended run also loses the tools that block on a person.
            let mut tools = filter_tools_for_stage(
                all_tool_defs,
                &stage.available_tools,
                &stage.required_tools,
                unattended,
            );
            let output = leviath_core::resolve_output_spec(
                blueprint.output.as_ref(),
                stage.output.as_ref(),
                output_request,
            );
            apply_output_shape(&mut tools, output.as_ref());
            Ok(ResolvedStage {
                provider_name: head.provider,
                model: head.model,
                tools,
                fallbacks: candidates,
                output,
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
            _r: &leviath_providers::InferenceRequest,
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
        assert!(p.infer(&request).await.is_err());
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
            fallback_order: Vec::new(),
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
            fallback_order: Vec::new(),
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
            fallback_order: Vec::new(),
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
            fallback_order: Vec::new(),
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
            false,
            None,
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
            false,
            None,
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
            false,
            None,
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
            fallback_order: Vec::new(),
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
            false,
            None,
        )
        .expect("anthropic is registered");
        let selected: Vec<&str> = resolved[0].tools.iter().map(|t| t.name.as_str()).collect();
        // `bash` resolved to `shell`; the unknown MCP name and unlisted
        // `read_file` were both excluded.
        assert_eq!(selected, vec!["shell"]);
    }

    // ── failover candidates (issue #201) ──────────────────────────────────

    /// `[(provider, model), ...]` for readable assertions.
    fn pairs(entries: &[ModelEntry]) -> Vec<(&str, &str)> {
        entries
            .iter()
            .map(|e| (e.provider.as_str(), e.model.as_str()))
            .collect()
    }

    #[test]
    fn candidates_keep_every_registered_entry_in_blueprint_order() {
        let cfg = model_cfg(vec![
            ("openrouter", "deepseek"),
            ("anthropic", "sonnet"),
            ("openai", "gpt"),
        ]);
        let registry = registry_with(&["openrouter", "anthropic", "openai"]);
        let got = resolve_stage_candidates(&cfg, None, &ModelDefaults::default(), &registry);
        // The head is what `resolve_stage_model` picks; the tail is where
        // failover goes. Before this, the tail was discarded at spawn.
        assert_eq!(
            pairs(&got),
            vec![
                ("openrouter", "deepseek"),
                ("anthropic", "sonnet"),
                ("openai", "gpt"),
            ]
        );
    }

    // ─── the unattended cut (issue #204) ─────────────────────────────────────

    /// A stage's tool defs for the three tools every one of these tests uses.
    fn ask_and_read_defs() -> Vec<Tool> {
        ["read_file", "ask_user_text", "ask_user_choice"]
            .iter()
            .map(|n| Tool {
                name: n.to_string(),
                description: String::new(),
                parameters: serde_json::Value::Null,
            })
            .collect()
    }

    fn names(tools: &[Tool]) -> Vec<&str> {
        tools.iter().map(|t| t.name.as_str()).collect()
    }

    #[test]
    fn an_attended_run_keeps_every_tool_the_stage_lists() {
        let available = vec![
            "read_file".to_string(),
            "ask_user_text".to_string(),
            "ask_user_choice".to_string(),
        ];
        let tools = filter_tools_for_stage(&ask_and_read_defs(), &available, &[], false);
        assert_eq!(
            names(&tools),
            vec!["read_file", "ask_user_text", "ask_user_choice"]
        );
    }

    #[test]
    fn candidates_skip_providers_that_are_not_registered() {
        let cfg = model_cfg(vec![("ghost", "nope"), ("anthropic", "sonnet")]);
        let registry = registry_with(&["anthropic"]);
        let got = resolve_stage_candidates(&cfg, None, &ModelDefaults::default(), &registry);
        assert_eq!(pairs(&got), vec![("anthropic", "sonnet")]);
    }

    #[test]
    fn the_global_chain_rescues_a_single_model_stage() {
        // The reported configuration: every stage names one OpenRouter model,
        // so the blueprint alone offers nowhere to fail over to.
        let cfg = ModelConfig {
            allow_user_default: false,
            ..model_cfg(vec![("openrouter", "deepseek")])
        };
        let defaults = ModelDefaults {
            fallback_order: vec![
                ModelEntry::new("anthropic".to_string(), "sonnet".to_string()),
                ModelEntry::new("ghost".to_string(), "nope".to_string()),
            ],
            ..Default::default()
        };
        let registry = registry_with(&["openrouter", "anthropic"]);
        let got = resolve_stage_candidates(&cfg, None, &defaults, &registry);
        assert_eq!(
            pairs(&got),
            vec![("openrouter", "deepseek"), ("anthropic", "sonnet")],
            "the unregistered global entry is skipped"
        );
    }

    #[test]
    fn the_global_chain_comes_after_the_user_default() {
        let cfg = model_cfg(vec![("openrouter", "deepseek")]);
        let defaults = ModelDefaults {
            provider: "anthropic".to_string(),
            model: Some("sonnet".to_string()),
            fallback_order: vec![ModelEntry::new("openai".to_string(), "gpt".to_string())],
        };
        let registry = registry_with(&["openrouter", "anthropic", "openai"]);
        let got = resolve_stage_candidates(&cfg, None, &defaults, &registry);
        // The user default heads the list (it is on `default_provider`), the
        // stage's own entry follows, and the host-wide chain is last - which is
        // the ordering this test exists to pin.
        assert_eq!(
            pairs(&got),
            vec![
                ("anthropic", "sonnet"),
                ("openrouter", "deepseek"),
                ("openai", "gpt"),
            ]
        );
    }

    #[test]
    fn the_default_provider_outranks_the_stages_own_list() {
        // `default_provider = "openrouter"` used to buy nothing: the bundled
        // blueprints all name anthropic/openai/ollama, ollama registers with no
        // key, so an OpenRouter-only install dispatched every stage at a
        // localhost server that was not running.
        let cfg = model_cfg(vec![
            ("anthropic", "claude-sonnet-5"),
            ("ollama", "qwen3.5:9b"),
        ]);
        let defaults = ModelDefaults {
            provider: "openrouter".to_string(),
            model: Some("openai/gpt-4o-mini".to_string()),
            ..Default::default()
        };
        let registry = registry_with(&["openrouter", "ollama"]);
        let got = resolve_stage_candidates(&cfg, None, &defaults, &registry);
        assert_eq!(
            pairs(&got),
            vec![
                ("openrouter", "openai/gpt-4o-mini"),
                ("ollama", "qwen3.5:9b"),
            ],
            "the user's default provider heads the list; the registered stage \
             entry stays behind it as a fallback"
        );
    }

    #[test]
    fn a_stage_that_forbids_the_user_default_keeps_its_own_order() {
        // `allow_user_default = false` is the existing way a blueprint pins its
        // provider, and it has to suppress the preference too - otherwise there
        // is no way left to say "this stage runs where I said".
        let cfg = ModelConfig {
            allow_user_default: false,
            ..model_cfg(vec![("anthropic", "sonnet"), ("openrouter", "deepseek")])
        };
        let defaults = ModelDefaults {
            provider: "openrouter".to_string(),
            model: Some("deepseek".to_string()),
            ..Default::default()
        };
        let registry = registry_with(&["openrouter", "anthropic"]);
        let got = resolve_stage_candidates(&cfg, None, &defaults, &registry);
        assert_eq!(
            pairs(&got),
            vec![("anthropic", "sonnet"), ("openrouter", "deepseek")]
        );
    }

    #[test]
    fn an_unregistered_default_provider_changes_nothing() {
        // The preference is over *registered* candidates only: a default
        // provider with no key must not reorder anything, and must certainly
        // not promote itself into the head where dispatch would park the run
        // on `ProviderMissing`.
        let cfg = model_cfg(vec![("anthropic", "sonnet"), ("ollama", "qwen")]);
        let defaults = ModelDefaults {
            provider: "openrouter".to_string(),
            model: Some("deepseek".to_string()),
            ..Default::default()
        };
        let registry = registry_with(&["anthropic", "ollama"]);
        let got = resolve_stage_candidates(&cfg, None, &defaults, &registry);
        assert_eq!(
            pairs(&got),
            vec![("anthropic", "sonnet"), ("ollama", "qwen")]
        );
    }

    #[test]
    fn candidates_are_deduplicated() {
        // The same pair arriving twice would spend a failover step going
        // nowhere, which reads to the operator as a swap that did nothing.
        let cfg = model_cfg(vec![("anthropic", "sonnet"), ("anthropic", "sonnet")]);
        let defaults = ModelDefaults {
            provider: "anthropic".to_string(),
            model: Some("sonnet".to_string()),
            fallback_order: vec![ModelEntry::new(
                "anthropic".to_string(),
                "sonnet".to_string(),
            )],
        };
        let registry = registry_with(&["anthropic"]);
        let got = resolve_stage_candidates(&cfg, None, &defaults, &registry);
        assert_eq!(pairs(&got), vec![("anthropic", "sonnet")]);
    }

    #[test]
    fn a_full_override_names_exactly_one_candidate() {
        // `--model provider/model` asked for that model, not a substitute.
        let cfg = model_cfg(vec![("anthropic", "sonnet"), ("openai", "gpt")]);
        let defaults = ModelDefaults {
            fallback_order: vec![ModelEntry::new("openai".to_string(), "gpt".to_string())],
            ..Default::default()
        };
        let registry = registry_with(&["anthropic", "openai", "ollama"]);
        let got = resolve_stage_candidates(&cfg, Some("ollama/llama"), &defaults, &registry);
        assert_eq!(pairs(&got), vec![("ollama", "llama")]);
    }

    #[test]
    fn a_bare_override_renames_the_model_on_every_candidate() {
        let cfg = model_cfg(vec![("anthropic", "sonnet"), ("openai", "gpt")]);
        let registry = registry_with(&["anthropic", "openai"]);
        let got =
            resolve_stage_candidates(&cfg, Some("haiku"), &ModelDefaults::default(), &registry);
        assert_eq!(
            pairs(&got),
            vec![("anthropic", "haiku"), ("openai", "haiku")]
        );
    }

    #[test]
    fn candidates_are_never_empty_even_with_nothing_registered() {
        // `resolve_stages` needs a name the user wrote to report against.
        let cfg = ModelConfig {
            allow_user_default: false,
            ..model_cfg(vec![("ghost", "nope")])
        };
        let got =
            resolve_stage_candidates(&cfg, None, &ModelDefaults::default(), &registry_with(&[]));
        assert_eq!(pairs(&got), vec![("ghost", "nope")]);
    }

    #[test]
    fn resolve_stages_carries_the_tail_onto_the_resolved_stage() {
        let mut stage = leviath_core::Stage::new(
            "work".to_string(),
            model_cfg(vec![("openrouter", "deepseek"), ("anthropic", "sonnet")]),
        );
        stage.available_tools = vec![];
        let layout = leviath_core::layout::ContextLayout::new(vec![], 1000);
        let bp = Blueprint::new("t".to_string(), "d".to_string(), vec![stage], layout);
        let registry = registry_with(&["openrouter", "anthropic"]);
        let resolved = resolve_stages(
            &bp,
            None,
            &ModelDefaults::default(),
            &registry,
            &[],
            false,
            None,
        )
        .expect("both providers are registered");
        assert_eq!(resolved[0].provider_name, "openrouter");
        assert_eq!(pairs(&resolved[0].fallbacks), vec![("anthropic", "sonnet")]);
    }

    #[test]
    fn an_unattended_run_loses_the_tools_that_wait_on_a_person() {
        // The whole point of issue #204: with nobody watching, a call to
        // `ask_user_text` can only park the agent, so the model never sees it.
        let available = vec![
            "read_file".to_string(),
            "ask_user_text".to_string(),
            "ask_user_choice".to_string(),
        ];
        let tools = filter_tools_for_stage(&ask_and_read_defs(), &available, &[], true);
        assert_eq!(names(&tools), vec!["read_file"]);
    }

    #[test]
    fn required_tools_survive_an_unattended_run() {
        // The opt-out: a stage that says it genuinely needs a person keeps the
        // named tool, and only that one.
        let available = vec![
            "read_file".to_string(),
            "ask_user_text".to_string(),
            "ask_user_choice".to_string(),
        ];
        let required = vec!["ask_user_text".to_string()];
        let tools = filter_tools_for_stage(&ask_and_read_defs(), &available, &required, true);
        assert_eq!(names(&tools), vec!["read_file", "ask_user_text"]);
    }

    #[test]
    fn a_required_tool_the_stage_never_offered_adds_nothing() {
        // `required_tools` narrows the unattended cut; it is not a second way to
        // grant a tool. (`Stage::validate` rejects this combination outright -
        // this is the belt to that pair of braces.)
        let available = vec!["read_file".to_string()];
        let required = vec!["ask_user_text".to_string()];
        let tools = filter_tools_for_stage(&ask_and_read_defs(), &available, &required, true);
        assert_eq!(names(&tools), vec!["read_file"]);
    }

    #[test]
    fn resolve_stages_applies_the_unattended_cut_per_stage() {
        // Two stages, one opting out, resolved in a single unattended run: the
        // cut is per stage, not per run.
        let mut plan =
            leviath_core::Stage::new("plan".to_string(), model_cfg(vec![("anthropic", "m")]));
        plan.available_tools = vec!["read_file".to_string(), "ask_user_text".to_string()];
        plan.required_tools = vec!["ask_user_text".to_string()];
        let mut build =
            leviath_core::Stage::new("build".to_string(), model_cfg(vec![("anthropic", "m")]));
        build.available_tools = vec!["read_file".to_string(), "ask_user_text".to_string()];
        let layout = leviath_core::layout::ContextLayout::new(vec![], 1000);
        let bp = Blueprint::new("t".to_string(), "d".to_string(), vec![plan, build], layout);

        let resolved = resolve_stages(
            &bp,
            None,
            &ModelDefaults::default(),
            &registry_with(&["anthropic"]),
            &ask_and_read_defs(),
            true,
            None,
        )
        .expect("anthropic is registered");

        assert_eq!(
            names(&resolved[0].tools),
            vec!["read_file", "ask_user_text"]
        );
        assert_eq!(names(&resolved[1].tools), vec!["read_file"]);
    }

    #[test]
    fn the_unattended_cut_resolves_aliases_on_both_sides() {
        // `edit_document` under an alias would be a hole in the cut, and a
        // `required_tools` entry written as an alias would be a hole in the
        // opt-out. Neither is: both sides canonicalise. `bash`/`shell` is the
        // only alias pair that exists, so it stands in for the mechanism - a
        // non-human tool is never cut whatever it is called.
        let defs = vec![Tool {
            name: "shell".to_string(),
            description: String::new(),
            parameters: serde_json::Value::Null,
        }];
        let available = vec!["bash".to_string()];
        let tools = filter_tools_for_stage(&defs, &available, &[], true);
        assert_eq!(names(&tools), vec!["shell"]);
    }

    // ── The output shape reaches the tool description ────────────────────────

    /// A helper mirroring how a stage that can submit is set up.
    fn output_stage_blueprint(
        agent: Option<leviath_core::output::OutputSpec>,
        stage_spec: Option<leviath_core::output::OutputSpec>,
    ) -> Blueprint {
        let mut stage =
            leviath_core::Stage::new("summary".to_string(), model_cfg(vec![("anthropic", "m")]));
        stage.available_tools = vec![leviath_tools::SUBMIT_OUTPUT_TOOL.to_string()];
        stage.output = stage_spec;
        let layout = leviath_core::layout::ContextLayout::new(vec![], 1000);
        let mut bp = Blueprint::new("t".to_string(), "d".to_string(), vec![stage], layout);
        bp.output = agent;
        bp
    }

    fn submit_tool_defs() -> Vec<Tool> {
        vec![Tool {
            name: leviath_tools::SUBMIT_OUTPUT_TOOL.to_string(),
            description: leviath_tools::submit_output_description(""),
            parameters: serde_json::Value::Null,
        }]
    }

    fn resolve_one(
        bp: &Blueprint,
        request: Option<&leviath_core::output::OutputSpec>,
    ) -> ResolvedStage {
        resolve_stages(
            bp,
            None,
            &ModelDefaults::default(),
            &registry_with(&["anthropic"]),
            &submit_tool_defs(),
            false,
            request,
        )
        .expect("anthropic is registered")
        .remove(0)
    }

    /// A stage can require an output without saying anything about its shape.
    /// There is nothing to paste into the description then, and the generic
    /// wording is what the model should read: an invented sentence about a
    /// format nobody declared would be worse than none.
    #[test]
    fn a_spec_that_says_nothing_leaves_the_description_generic() {
        let generic = leviath_tools::submit_output_description("");
        let bp = output_stage_blueprint(None, Some(leviath_core::output::OutputSpec::default()));

        let resolved = resolve_one(&bp, None);

        assert_eq!(
            resolved
                .tools
                .iter()
                .find(|t| t.name == leviath_tools::SUBMIT_OUTPUT_TOOL)
                .expect("the stage offers the tool")
                .description,
            generic
        );
    }

    /// The whole mechanism for arbitrary formats: a label this crate has never
    /// heard of is pasted into the description the model reads, with no parsing
    /// and no per-format branch anywhere.
    #[test]
    fn an_unrecognized_format_reaches_the_submit_tool_description() {
        let bp = output_stage_blueprint(
            None,
            Some(leviath_core::output::OutputSpec {
                format: Some("a2ui".to_string()),
                instructions: Some("One card per finding.".to_string()),
                example: Some("{\"root\": {}}".to_string()),
                schema: None,
                validator: None,
            }),
        );
        let resolved = resolve_one(&bp, None);
        let description = &resolved
            .tools
            .iter()
            .find(|t| t.name == leviath_tools::SUBMIT_OUTPUT_TOOL)
            .expect("the stage offers the tool")
            .description;
        assert!(description.contains("a2ui"), "{description}");
        assert!(
            description.contains("One card per finding."),
            "{description}"
        );
        assert!(description.contains("{\"root\": {}}"), "{description}");
        assert_eq!(
            resolved.output.and_then(|s| s.format).as_deref(),
            Some("a2ui")
        );
    }

    /// A stage that declares nothing keeps the generic wording rather than
    /// growing an empty shape paragraph.
    #[test]
    fn a_stage_declaring_no_shape_keeps_the_generic_description() {
        let bp = output_stage_blueprint(None, None);
        let resolved = resolve_one(&bp, None);
        assert_eq!(
            resolved.tools[0].description,
            leviath_tools::submit_output_description("")
        );
        assert!(resolved.output.is_none());
    }

    /// The caller's request wins over the blueprint, and naming a format
    /// without a schema retires the one the blueprint declared: a check written
    /// for one shape says nothing about another.
    #[test]
    fn a_callers_request_overrides_the_blueprint_and_drops_its_schema() {
        let bp = output_stage_blueprint(
            Some(leviath_core::output::OutputSpec {
                format: Some("json".to_string()),
                schema: Some(serde_json::json!({"type": "object"})),
                ..Default::default()
            }),
            None,
        );
        let request = leviath_core::output::OutputSpec {
            format: Some("xml".to_string()),
            ..Default::default()
        };
        let resolved = resolve_one(&bp, Some(&request));
        let spec = resolved.output.expect("a shape was asked for");
        assert_eq!(spec.format.as_deref(), Some("xml"));
        assert_eq!(spec.schema, None);
        assert!(resolved.tools[0].description.contains("xml"));
    }

    /// A stage that never offers the tool is untouched, which is most stages.
    #[test]
    fn a_stage_without_the_submit_tool_is_left_alone() {
        let mut stage =
            leviath_core::Stage::new("plan".to_string(), model_cfg(vec![("anthropic", "m")]));
        stage.available_tools = vec!["read_file".to_string()];
        let layout = leviath_core::layout::ContextLayout::new(vec![], 1000);
        let mut bp = Blueprint::new("t".to_string(), "d".to_string(), vec![stage], layout);
        bp.output = Some(leviath_core::output::OutputSpec {
            format: Some("a2ui".to_string()),
            ..Default::default()
        });
        let resolved = resolve_stages(
            &bp,
            None,
            &ModelDefaults::default(),
            &registry_with(&["anthropic"]),
            &[Tool {
                name: "read_file".to_string(),
                description: "read a file".to_string(),
                parameters: serde_json::Value::Null,
            }],
            false,
            None,
        )
        .expect("anthropic is registered");
        assert_eq!(resolved[0].tools[0].description, "read a file");
    }
}
