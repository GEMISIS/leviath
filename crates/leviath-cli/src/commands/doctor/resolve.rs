//! Check 2 of `lev doctor`: what the user's config alone resolves to.
//!
//! Split out of `doctor.rs` by concern, like `resolve_notes`: the verdict on
//! the config's provider preference, and the model picked to probe with when
//! the config names none of its own.

use super::*;

use crate::daemon::spawn::model_defaults;
use leviath_runtime::pipeline::{ModelDefaults, model_key, providers_tried, resolve_stage_model};

/// What check 2 settled on, when it settled on something usable. The handle is
/// carried rather than looked up again so the later checks cannot disagree with
/// the one that reported.
pub(super) struct Resolved {
    pub(super) provider_name: String,
    /// The model the chain named. `None` when the config names no model of
    /// its own, in which case the checks that need one pick it from the
    /// provider's catalogue ([`probe_model`]).
    pub(super) model: Option<String>,
    pub(super) provider: Arc<dyn Provider>,
}

/// Run the real stage-model fallback chain against an **empty** [`ModelChoice`],
/// so what comes back is what the user's config alone would pick for a stage
/// that states no preference of its own.
///
/// The chain's last resort is unchecked: with nothing to pick from it hands
/// back an empty candidate, no provider and no model, which nothing answers
/// to. That is not a finding about the user's config, so it is never
/// reported as one. Reaching it means the config names no model of its own,
/// and the verdict is then about the provider preference instead
/// ([`preference_check`]): a preferred provider that is registered passes,
/// with the probe model chosen later from its catalogue, and none registered
/// is the loud failure a fresh install with no provider deserves.
///
/// Resolving through [`ProviderRegistry::get`] rather than `has` makes the same
/// decision (native first, then the script layer) while keeping the handle, so
/// the provider cannot go missing between deciding to use it and using it.
pub(super) fn resolve_check(
    config: &Config,
    model_override: Option<&str>,
    registry: &ProviderRegistry,
) -> (Check, Option<Resolved>) {
    let empty = ModelChoice::default();
    let defaults = model_defaults(config);
    let (provider_name, model) = resolve_stage_model(&empty, model_override, &defaults, registry);

    // In the order as well as registered: a `--model provider/model` names
    // its provider outright, and one left out of the order is not set up.
    let set_up = registry
        .get(&provider_name)
        .filter(|_| defaults.is_preferred(&provider_name));
    if let Some(provider) = set_up {
        return (
            Check::ok(
                "resolve",
                format!(
                    "{provider_name} / {model}{}{}",
                    head_provider_note(config, &provider_name, model_override, registry),
                    qualified_user_model_notes(config, model_override),
                ),
            ),
            Some(Resolved {
                provider_name,
                model: Some(model),
                provider,
            }),
        );
    }
    // A `--model` names its own provider (`provider/model`) or pairs a bare id
    // with the first provider in the order, and either can name one that is
    // not here. The
    // chain reported exactly that provider, so the answer is about it.
    if model_override.is_some() {
        let why = match registry.has(&provider_name) {
            true => "which is configured but not in `[providers] provider_order`, so it \
                     is not set up. List it with `lev providers order <names, best first>`."
                .to_string(),
            false => format!(
                "which is not configured (tried: {}). Configure it with `lev setup`, or \
                 add it to config.toml.",
                providers_tried(&empty, model_override, &defaults)
            ),
        };
        return (
            Check::fail("resolve", format!("resolved to '{provider_name}', {why}")),
            None,
        );
    }
    preference_check(config, &defaults, registry)
}

/// The verdict when the chain found nothing of the user's own to send: judge
/// the provider preference, which is what a real run routes by.
///
/// Three answers. A preferred provider is registered and no model setting is
/// in play: pass, and let the later checks pick a model from its catalogue,
/// because that is the state of every fresh install with one provider and
/// nothing wrong with it. A preferred provider is registered but
/// `override_model` or `fallback_model` is set: those pair with the first
/// provider in the order, which must then be the one that is not configured,
/// and the message names that. Nothing preferred is registered: the plain
/// failure, said in terms of the providers the user listed, or of the empty
/// order when they listed none.
fn preference_check(
    config: &Config,
    defaults: &ModelDefaults,
    registry: &ProviderRegistry,
) -> (Check, Option<Resolved>) {
    let preferred = defaults.order();
    let preference = preferred.join(", ");
    let mut registered = registry.provider_names();
    registered.sort_unstable();
    let registered = match registered.is_empty() {
        true => "none".to_string(),
        false => registered.join(", "),
    };
    let names_a_model = config.override_model.is_some() || config.fallback_model.is_some();
    match preferred
        .iter()
        .find_map(|name| registry.get(name).map(|provider| (*name, provider)))
    {
        Some((name, provider)) if !names_a_model => (
            Check::ok(
                "resolve",
                format!(
                    "{name}  (note: this check resolves no blueprint, and with no \
                     `override_model` or `fallback_model` set your config names no model \
                     of its own, so the inference check picks one from '{name}'. A real \
                     run is different: each stage runs the model its blueprint names, \
                     routed through your provider preference [{preference}]. Set \
                     `override_model` only to pin one model across every stage.)"
                ),
            ),
            Some(Resolved {
                provider_name: name.to_string(),
                model: None,
                provider,
            }),
        ),
        Some((name, _)) => (
            Check::fail(
                "resolve",
                format!(
                    "`override_model` and `fallback_model` pair with the first provider \
                     in provider_order, '{}', which is not configured (registered: \
                     {registered}). Configure it with `lev setup`, or put a configured \
                     provider such as '{name}' first with `lev providers order`.",
                    preferred[0]
                ),
            ),
            None,
        ),
        None => {
            let named = match preferred.is_empty() {
                true => "no provider is set up, `[providers] provider_order` is empty".to_string(),
                false => format!("nothing in provider_order = [{preference}] is configured"),
            };
            (
                Check::fail(
                    "resolve",
                    format!(
                        "no configured provider to run on: {named} (registered: \
                         {registered}). Run `lev setup` to add a provider, or list a \
                         configured one with `lev providers order <name>`."
                    ),
                ),
                None,
            )
        }
    }
}

/// A model to probe a provider with when the config names none of its own.
///
/// The catalogue first: [`Provider::served_catalog`] is the complete list a
/// provider commits to, already narrowed to models a request may name. A
/// provider that cannot say is asked to list instead, and the first entry its
/// own table also knows wins, because a listing (OpenAI's, measured) carries
/// embedding and speech models too, and a probe sent to one of those fails for
/// a reason that says nothing about the credential. Nothing at all is
/// `Ok(None)`, and the caller says so rather than guessing a name. A listing
/// that failed is the error itself, which is the finding.
pub(super) async fn probe_model(
    provider: &dyn Provider,
) -> Result<Option<String>, leviath_providers::ProviderError> {
    if let Some(first) = provider
        .served_catalog()
        .and_then(|catalog| catalog.into_iter().next())
    {
        return Ok(Some(first));
    }
    let listed = provider.list_models().await?;
    Ok(listed
        .iter()
        .find(|m| provider.serves_model(model_key(&m.id)).is_some())
        .or_else(|| listed.first())
        .map(|m| m.id.clone()))
}
