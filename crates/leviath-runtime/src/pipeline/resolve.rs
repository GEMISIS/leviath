//! Stage model and tool resolution: turning a blueprint's per-stage
//! [`ModelConfig`] and `available_tools` into concrete [`ResolvedStage`]s
//! against whatever providers and tools the host actually has.
//!
//! Lives in the runtime rather than the CLI daemon so an embedding host
//! resolves stages exactly the way `lev run` does. The one policy input that
//! comes from the CLI's config file - the user's default provider/model -
//! arrives as a plain [`ModelDefaults`] value.

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
    /// that provider stops answering. The case it covers: every stage names a
    /// single OpenRouter model, so there is nothing to fall back to when the
    /// account runs out of credits.
    pub fallback_order: Vec<ModelEntry>,
}

/// Resolve a stage's [`ModelConfig`] to a concrete `(provider, model)` against
/// the registered providers. Honors a `--model` override (`provider/model` or a
/// bare `model`), otherwise picks the first listed model whose provider is
/// registered, then falls back to the user default (when `allow_user_default`),
/// and finally to the config's first listed entry.
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
/// fails over to when a provider turns out to be unusable mid-run. Consult the
/// ordered list in `ModelConfig.models` only for *registration* at spawn time
/// and a provider that is configured but out of credits gets picked and then
/// never abandoned.
///
/// Order: the stage's own registered entries, then the user default, then the
/// host-wide `fallback_order`. Deduplicated, because the same pair reaching the
/// list twice would spend a failover step going nowhere. Never empty: with
/// nothing registered it yields the blueprint's own first entry, exactly as
/// A model's identity, independent of the route it is reached by.
///
/// The same model is spelled differently depending on the provider serving it:
/// `gpt-5.5` on OpenAI is `openai/gpt-5.5` on OpenRouter, and
/// `claude-opus-5` is `anthropic/claude-opus-5`. Comparing the raw strings
/// makes one model look like two, which is what lets a route preference be read
/// as a model preference.
///
/// The last segment is the model; anything before it is the vendor namespace a
/// gateway prefixes. A local model with no slash (`qwen3.5:9b`) is already its
/// own key.
///
/// Public because every layer that compares two model names has to compare them
/// the same way: `lev validate` and the provider registry call this rather than
/// each keeping a copy of "what counts as the same model", which would not stay
/// equal.
pub fn model_key(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// before, and `resolve_stages` rejects that unusable case with a clear error.
pub(crate) fn resolve_stage_candidates(
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

    // Every listed model, in blueprint order. A bare `--model` override renames
    // the model but keeps the order.
    //
    // An entry with no provider names a model and leaves the route open: ask
    // the registered providers which of them serves it. That is the point of
    // letting a blueprint omit the provider - the author knows which model the
    // stage needs, and only the machine knows how to reach it.
    for entry in &model_cfg.models {
        let model = override_model
            .clone()
            .unwrap_or_else(|| entry.model.clone());
        if !entry.provider.is_empty() {
            if !registry.has(&entry.provider) {
                continue;
            }
            // A pinned pair is the author naming a route, and it is taken on
            // trust: a provider whose catalogue this build cannot see would
            // otherwise have every pin to it thrown away.
            //
            // A *renamed* pair is nobody's route. A bare `--model` override
            // replaces the model on every entry while leaving each entry's
            // provider alone, so `--model gpt-5.5` against a stage listing
            // `{provider = "anthropic", ...}` produces `anthropic/gpt-5.5` - a
            // pair no one asked for and no provider serves. It sits in the
            // fallback list until a failover reaches it, and then the run moves
            // itself onto a route that cannot answer. So a renamed pair has to
            // earn its place by the same test an open route does.
            let renamed = override_model.is_some() && model != entry.model;
            // Dropped only on a definite no. A provider that claims nothing -
            // one whose catalogue this build cannot see, or a script that never
            // primed - cannot tell us the pair is wrong, and treating silence as
            // rejection would throw away every route to it. So the question is
            // asked of the registry as a whole: if *somebody* serves the renamed
            // model, then who serves it is knowable, and a provider that does
            // not is definitively the wrong place to send it. If nobody claims
            // it, nothing here knows better than the entry already did.
            let usable = !renamed
                || !registry
                    .native_providers()
                    .into_iter()
                    .any(|(_, p)| p.serves_model(model_key(&model)).is_some())
                || registry
                    .native_providers()
                    .into_iter()
                    .find(|(name, _)| *name == entry.provider)
                    .is_some_and(|(_, provider)| {
                        provider.serves_model(model_key(&model)).is_some()
                    });
            if usable {
                push(entry.provider.clone(), model);
            } else {
                tracing::warn!(
                    provider = %entry.provider,
                    model = %model,
                    listed_model = %entry.model,
                    "the --model override renamed this entry onto a provider that \
                     does not serve it, so the pair is skipped rather than left \
                     for a failover to land on"
                );
            }
            continue;
        }
        let key = model_key(&model);
        // Default provider first, so an open route follows the same preference
        // a named model's routes do.
        let mut candidates_for_model = registry.native_providers();
        candidates_for_model.sort_by_key(|(name, _)| *name != defaults.provider);
        // A script provider is not in `native_providers` - it is compiled on
        // demand rather than enumerated - so without this an open route can
        // never land on one, and a local box serving one fast model is
        // unreachable however the machine sets `default_provider`. The *default*
        // provider is asked as well, which costs one compile of a script this
        // machine has explicitly chosen, and leaves every other script on disk
        // untouched. Put first, because being the default is what it means.
        if let Some(script) = registry
            .script_provider_named(&defaults.provider)
            .filter(|_| model_cfg.allow_user_default)
        {
            candidates_for_model.insert(0, (defaults.provider.as_str(), script));
        }
        let mut routed = false;
        for (name, provider) in candidates_for_model {
            // A provider that may only be reached by name does not win an open
            // route. Selecting one changes what gets billed - a subscription
            // transport spends a plan rather than an API balance - so enabling
            // it must not silently move every bare-named stage onto it. It is
            // still reachable by an explicit `provider/model`, a fallback
            // entry, or being the default, which is why the default-provider
            // case is exempt.
            if provider.explicit_route_only() && name != defaults.provider {
                continue;
            }
            if let Some(id) = provider.serves_model(key) {
                push(name.to_string(), id);
                routed = true;
            }
        }
        // Say so when nothing serves it. The chain carries on to the next model,
        // which is what a fallback list is for, but an unroutable name is worth
        // seeing: it is equally a typo, a model no configured provider carries,
        // or a gateway whose catalogue never primed.
        if !routed {
            tracing::warn!(
                model = %model,
                "no configured provider serves this model, so it is skipped;                  the stage falls through to the next model listed"
            );
        }
    }

    let user_default = user_default_model(model_cfg, override_model.as_deref(), defaults, registry);
    if let Some((provider, model)) = &user_default {
        push(provider.clone(), model.clone());
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
    // the front, the user's own `default_model` first among them and the rest
    // in blueprint order. The default model leads because it is the one the
    // user named: every bundled blueprint lists Ollama as `qwen3.5:9b`, and
    // someone who set `default_model = "qwen3.8:latest"` was still sent to the
    // blueprint's model, which they may never have pulled. A blueprint that
    // must pin its own provider already has the way to say so -
    // `allow_user_default = false` - and that suppresses this too.
    if model_cfg.allow_user_default && registry.has(&defaults.provider) {
        // Grouped by MODEL, not by provider. `default_provider` says where a
        // run should go, which is a statement about routes; letting it reorder
        // across models turns it into a statement about which model to use, and
        // a blueprint that deliberately chose one per stage silently gets
        // another.
        //
        // Models keep blueprint order, except that the user's own
        // `default_model` leads - that IS a model preference, and someone who
        // named a model meant it.
        let default_model = user_default.as_ref().map(|(_, m)| m.as_str());
        let mut order: Vec<String> = Vec::new();
        for c in &candidates {
            let key = model_key(&c.model).to_string();
            if !order.contains(&key) {
                order.push(key);
            }
        }
        if let Some(dm) = default_model
            && let Some(at) = order.iter().position(|k| k == model_key(dm))
        {
            let key = order.remove(at);
            order.insert(0, key);
        }
        let mut grouped: Vec<ModelEntry> = Vec::with_capacity(candidates.len());
        for key in order {
            let mut group: Vec<ModelEntry> = candidates
                .iter()
                .filter(|c| model_key(&c.model) == key)
                .cloned()
                .collect();
            // Stable: the default provider's route to this model first, the
            // blueprint's order behind it.
            group.sort_by_key(|c| c.provider != defaults.provider);
            grouped.extend(group);
        }
        candidates = grouped;
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
    //
    // A pair whose provider is here and says it does not carry that model is a
    // phantom of the same kind: the failover step lands on it, the API refuses
    // the id, and the run has spent a step going nowhere. The head is not
    // filtered here - `resolve_stages` refuses it outright, because a stage
    // silently running the next model down is the thing being fixed.
    let tail: Vec<ModelEntry> = candidates
        .split_off(1)
        .into_iter()
        .filter(|e| registry.has(&e.provider) && !registry.refuses_model(&e.provider, &e.model))
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
        let model = bare_default_model(&defaults.provider, default_model);
        return Some((defaults.provider.clone(), model.to_string()));
    }
    None
}

/// The model id a `default_model` setting actually names, with a leading
/// `<default_provider>/` taken off.
///
/// `default_model` is a bare model id that pairs with `default_provider`, but
/// it is easy to write qualified: `--model` and `[providers] fallback_order`
/// both take `provider/model`, an OpenRouter id such as
/// `deepseek/deepseek-v4-flash` already looks qualified, and a console that
/// lists models as `provider/id` hands the pair back in one string. Sent
/// verbatim, `default_provider = "ollama"` with `default_model =
/// "ollama/qwen3.8:latest"` reached Ollama as a request for a model called
/// `ollama/qwen3.8:latest`, which it does not have. The prefix names the
/// provider the setting already names, so dropping it loses nothing.
///
/// The one catalog that can legitimately start an id with its own name is
/// OpenRouter's (`openrouter/auto` is its router). Those ids have no further
/// `/`, while a mistakenly qualified OpenRouter model always does
/// (`openrouter/deepseek/deepseek-v4-flash`), which is how the two are told
/// apart. Anything that does not begin with the provider's name is returned as
/// written.
pub fn bare_default_model<'a>(provider: &str, model: &'a str) -> &'a str {
    let Some(rest) = model
        .strip_prefix(provider)
        .and_then(|rest| rest.strip_prefix('/'))
        .filter(|rest| !rest.is_empty())
    else {
        return model;
    };
    if provider == "openrouter" && !rest.contains('/') {
        return model;
    }
    rest
}

/// Which MCP server advertises each tool: advertised name -> server name.
///
/// Built where the servers are registered, because that is the only place the
/// mapping exists. A [`Tool`] carries a name, a description and a schema, and
/// the advertised name does not reliably contain the server: `leviath-mcp`
/// prefers the bare tool name and only prefixes with the server on a collision,
/// so `github`'s `create_issue` is usually advertised as `create_issue`. There
/// is no string pattern that answers "does this tool belong to github", which
/// is why a grant names the server and this table answers for it.
pub type ToolOwners = std::collections::HashMap<String, String>;

/// Every tool a run could offer, and which MCP server each came from.
///
/// The two travel together because a connector grant needs both: the defs to
/// filter, and the ownership to know what a server's name covers. Passed as one
/// value rather than two adjacent `&`s - `resolve_stages` was already at the
/// argument-count limit, and two references of different types next to each
/// other is exactly the pair a reader transposes.
#[derive(Clone, Copy)]
pub struct ToolCatalog<'a> {
    /// Every tool definition available to the run.
    pub defs: &'a [Tool],
    /// Which MCP server advertises each of them. Empty for a run with no MCP
    /// servers, which makes every connector grant resolve to nothing.
    pub owners: &'a ToolOwners,
}

/// A stage's Layer-1 grant list: the names it asked for, plus every tool
/// belonging to a server it named.
///
/// Connector grants are resolved here rather than at parse time because the
/// answer only exists once the servers are connected - which is the whole point
/// of naming a server instead of its tools. A connector that resolves to
/// nothing (server not installed, or not connected this run) contributes
/// nothing, exactly as an `available_tools` name matching nothing does.
///
/// Order is `available_tools` first, then each connector's tools in the order
/// the servers were named, and the result is de-duplicated: a tool named
/// individually *and* covered by a connector is granted once.
pub fn expand_connector_grants(
    available: &[String],
    connectors: &[String],
    owners: &ToolOwners,
) -> Vec<String> {
    if connectors.is_empty() {
        return available.to_vec();
    }
    let mut granted: Vec<String> = available.to_vec();
    for server in connectors {
        // Sorted so a stage's advertised tool order does not depend on hash
        // iteration - the model sees this list, and a set that reshuffles
        // between runs is a difference nobody can explain.
        let mut owned: Vec<&String> = owners
            .iter()
            .filter(|(_, owner)| *owner == server)
            .map(|(tool, _)| tool)
            .collect();
        owned.sort();
        for tool in owned {
            if !granted.contains(tool) {
                granted.push(tool.clone());
            }
        }
    }
    granted
}

/// Filter `all` tool defs down to those a stage's `available_tools` names
/// (alias-resolved). Shared by spawn-time stage resolution and the mid-run
/// tool-service refresh so both apply Layer-1 identically.
///
/// Connector grants are expanded into `available` by
/// [`expand_connector_grants`] before this sees it, so both callers apply one
/// rule and this stays an exact-match filter.
pub(crate) fn filter_tools_by_available(all: &[Tool], available: &[String]) -> Vec<Tool> {
    if available.is_empty() {
        return Vec::new();
    }
    let wanted: std::collections::HashSet<&str> = available
        .iter()
        .map(|n| leviath_tools::canonical_tool_name(n))
        .collect();
    // Names that match nothing get one more chance, as a server-qualified MCP
    // tool. Computed once rather than per candidate, and only from the names
    // that actually missed - so a stage whose grants all resolve does no extra
    // work.
    let unmatched: Vec<&str> = wanted
        .iter()
        .copied()
        .filter(|n| !all.iter().any(|t| t.name == *n))
        .collect();
    let recovered = recover_unqualified(all, &unmatched);
    all.iter()
        .filter(|t| wanted.contains(t.name.as_str()) || recovered.contains(t.name.as_str()))
        .cloned()
        .collect()
}

/// MCP tools named without their server, matched to the qualified name when
/// exactly one server offers them.
///
/// MCP tools are advertised as `<server>__<tool>`, so a blueprint written
/// against bare names says `create_issue` where the tool is
/// `github__create_issue`. Left alone that grant matches nothing and the tool
/// is silently not offered.
///
/// Three things keep this from doing harm:
///
/// - It only ever sees names that matched **nothing**. A built-in `read_file`
///   matches itself, so a server that also offers `read_file` can never capture
///   the grant.
/// - It requires exactly one candidate. Two servers offering `create_issue`
///   leave the name unresolved, because it genuinely is ambiguous and the
///   manifest has to say which. A silent "whichever registered first" is what
///   qualified names exist to prevent.
/// - It matches on the qualified shape, so `create_issue` finds
///   `github__create_issue` and not some unrelated tool ending in those
///   characters.
fn recover_unqualified<'a>(
    all: &'a [Tool],
    unmatched: &[&str],
) -> std::collections::HashSet<&'a str> {
    let mut recovered = std::collections::HashSet::new();
    for name in unmatched {
        let suffix = format!("__{name}");
        let mut candidates = all
            .iter()
            .filter(|t| t.name.ends_with(&suffix) && t.name.len() > suffix.len());
        let (Some(only), None) = (candidates.next(), candidates.next()) else {
            continue;
        };
        tracing::debug!(
            wrote = %name,
            resolved = %only.name,
            "a stage names an MCP tool without its server; resolved because exactly one offers it"
        );
        recovered.insert(only.name.as_str());
    }
    recovered
}

/// The stage's Layer-1 tool set for a run that may have nobody watching.
///
/// Same filter as `filter_tools_by_available`, then - for an unattended run -
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
/// a stage can resolve to a provider that does not exist, and the agent spawns
/// anyway: `Active`, iteration 0, unable to take a single turn for as long as
/// the host lives. Catching it here turns a silently wedged run into an error
/// the caller sees.
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
    catalog: ToolCatalog<'_>,
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
                     with `lev setup`, or add it to config.toml; the next run \
                     picks it up, with no daemon restart.",
                    stage.name,
                    providers_tried(&stage.model, model_override, defaults)
                ));
            }
            // The provider is here and says it does not carry this model. That
            // is a different failure from the one above and needs its own end:
            // the provider answers, so nothing fails over, and the run would
            // spend its whole life posting a model id the API rejects.
            //
            // Refused rather than skipped on purpose. Dropping the head and
            // promoting the next entry is what the blueprint author cannot see
            // - a stage they pinned to one model quietly running another - and
            // it is the behaviour this check exists to end. Only a provider
            // that published a complete catalogue can get here, so the answer
            // is evidence rather than an absence of it (`refuses_model`).
            if registry.refuses_model(&head.provider, &head.model) {
                // The provider gets to say *why* first. "Does not serve it"
                // is right for a typo and wrong for a model the route carries
                // and the account cannot reach - and those two send a reader
                // to different places, one to check the spelling and one to
                // change the stage or the plan.
                return Err(match registry.refusal_reason(&head.provider, &head.model) {
                    Some(reason) => format!(
                        "stage '{}' names {}/{}: {reason}",
                        stage.name, head.provider, head.model
                    ),
                    None => format!(
                        "stage '{}' names {}/{}, which provider '{}' does not serve. \
                         Run `lev models list --provider {}` to see what it carries, \
                         then name one of those in the stage's models list.",
                        stage.name, head.provider, head.model, head.provider, head.provider,
                    ),
                });
            }
            // Empty `available_tools` exposes no tools; otherwise filter the full
            // set by name (alias-resolved). A name matching nothing (a typo, or an
            // MCP tool whose server isn't installed) is simply omitted. An
            // unattended run also loses the tools that block on a person.
            let granted = expand_connector_grants(
                &stage.available_tools,
                &stage.available_connectors,
                catalog.owners,
            );
            let mut tools =
                filter_tools_for_stage(catalog.defs, &granted, &stage.required_tools, unattended);
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
    /// A catalog over `defs` with no MCP servers behind it, which is what every
    /// test that is not about connectors wants.
    fn catalog(defs: &[Tool]) -> ToolCatalog<'_> {
        // A leaked empty map: `ToolCatalog` borrows, and a local would not
        // outlive the call in the expression position these are used in.
        static EMPTY: std::sync::OnceLock<ToolOwners> = std::sync::OnceLock::new();
        ToolCatalog {
            defs,
            owners: EMPTY.get_or_init(ToolOwners::new),
        }
    }

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

    /// A stage config whose entries name models and leave the route open.
    fn model_cfg_open(models: Vec<&str>) -> ModelConfig {
        ModelConfig {
            models: models
                .into_iter()
                .map(|m| ModelEntry::new(String::new(), m.to_string()))
                .collect(),
            allow_user_default: true,
            parameters: HashMap::new(),
            request_timeout_secs: None,
        }
    }

    /// A registry whose only provider is a script one, named as the default.
    ///
    /// Built from a real script on disk because that is the only way to reach
    /// the lazy-resolve path: a script provider is compiled on demand rather
    /// than registered, which is exactly why it was invisible to an open route.
    fn registry_with_script_default(models: &[&str]) -> (ProviderRegistry, tempfile::TempDir) {
        use crate::script_provider::ScriptProviderLayer;
        let dir = tempfile::tempdir().unwrap();
        let listed = models
            .iter()
            .map(|m| {
                format!(
                    "#{{ id: \"{m}\", display_name: \"{m}\", max_context_tokens: 4096, \
                     max_output_tokens: 512 }}"
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            dir.path().join("spark.rhai"),
            format!(
                "fn initialize(config) {{ #{{}} }}\n\
                 fn inference(state, request) {{ #{{ content: \"ok\" }} }}\n\
                 fn list_models(state) {{ [ {listed} ] }}"
            ),
        )
        .unwrap();
        let layer = ScriptProviderLayer::new(
            dir.path().to_path_buf(),
            HashMap::new(),
            HashMap::new(),
            None,
            Vec::new(),
        );
        (
            ProviderRegistry::new().with_script_layer(Arc::new(layer)),
            dir,
        )
    }

    /// A stage naming a model with no provider resolves to the script provider
    /// the machine named as its default.
    ///
    /// `native_providers` does not enumerate script providers, so asking it
    /// alone sends the stage elsewhere even when a local box serves exactly
    /// the model the blueprint asked for.
    #[tokio::test]
    async fn a_script_provider_named_as_default_wins_an_open_route() {
        let (registry, _dir) = registry_with_script_default(&["local-fast"]);
        registry
            .prime_capabilities(std::time::Duration::from_secs(5), &["spark"])
            .await;
        let defaults = ModelDefaults {
            provider: "spark".to_string(),
            model: None,
            fallback_order: Vec::new(),
        };

        let picked = resolve_stage_candidates(
            &model_cfg_open(vec!["local-fast"]),
            None,
            &defaults,
            &registry,
        );

        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].provider, "spark");
        assert_eq!(picked[0].model, "local-fast");
    }

    /// The preference decides the route, not the model, so a blueprint that
    /// tiers its stages keeps each stage's own model. This is what separates it
    /// from `default_model`, which pins one model everywhere.
    #[tokio::test]
    async fn preferring_a_script_provider_keeps_each_stage_on_its_own_model() {
        let (registry, _dir) = registry_with_script_default(&["cheap-model", "costly-model"]);
        registry
            .prime_capabilities(std::time::Duration::from_secs(5), &["spark"])
            .await;
        let defaults = ModelDefaults {
            provider: "spark".to_string(),
            model: None,
            fallback_order: Vec::new(),
        };

        for model in ["cheap-model", "costly-model"] {
            let picked =
                resolve_stage_candidates(&model_cfg_open(vec![model]), None, &defaults, &registry);
            assert_eq!(picked[0].provider, "spark");
            assert_eq!(picked[0].model, model, "each stage keeps its own model");
        }
    }

    /// A stage that pins its own provider opts out of the machine's preference,
    /// and that has to hold for a script provider too.
    #[tokio::test]
    async fn a_stage_that_refuses_the_user_default_does_not_get_the_script() {
        let (registry, _dir) = registry_with_script_default(&["local-fast"]);
        registry
            .prime_capabilities(std::time::Duration::from_secs(5), &["spark"])
            .await;
        let defaults = ModelDefaults {
            provider: "spark".to_string(),
            model: None,
            fallback_order: Vec::new(),
        };
        let mut cfg = model_cfg_open(vec!["local-fast"]);
        cfg.allow_user_default = false;

        let picked = resolve_stage_candidates(&cfg, None, &defaults, &registry);

        // Nothing else serves it, so the stage falls through to the blueprint's
        // own first entry rather than silently landing on the script.
        assert!(picked.iter().all(|c| c.provider != "spark"), "{picked:?}");
    }

    fn registry_with(providers: &[&str]) -> ProviderRegistry {
        let mut r = ProviderRegistry::new();
        for p in providers {
            r.register(p.to_string(), Arc::new(FakeProvider::default()));
        }
        r
    }

    /// A registry whose providers each serve a named set of models, for the
    /// entries that name a model and leave the route open.
    fn registry_serving(providers: &[(&str, &[&str])]) -> ProviderRegistry {
        let mut r = ProviderRegistry::new();
        for (name, models) in providers {
            r.register(
                (*name).to_string(),
                Arc::new(FakeProvider {
                    serves: models.iter().map(|m| (*m).to_string()).collect(),
                    catalog: None,
                    refusal: None,
                }),
            );
        }
        r
    }

    #[derive(Default)]
    struct FakeProvider {
        /// Models this provider claims. Empty is the ordinary fixture: the
        /// resolver only asks `has()` for a route-pinned entry.
        serves: Vec<String>,
        /// The complete catalogue this provider publishes, if it publishes one.
        /// `None` is the ordinary fixture and means "will not say", which no
        /// caller may read as a refusal.
        catalog: Option<Vec<String>>,
        /// What it says about refusing something outside that catalogue.
        refusal: Option<String>,
    }
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
        fn serves_model(&self, model_key: &str) -> Option<String> {
            self.serves
                .iter()
                .any(|m| m == model_key)
                .then(|| model_key.to_string())
        }
        fn served_catalog(&self) -> Option<Vec<String>> {
            self.catalog.clone()
        }

        fn refusal_reason(&self, _model_key: &str) -> Option<String> {
            self.refusal.clone()
        }
    }

    /// A registry whose providers each publish a complete catalogue, for the
    /// checks that may only refuse a pair on a definite no.
    fn registry_publishing(providers: &[(&str, &[&str])]) -> ProviderRegistry {
        let mut r = ProviderRegistry::new();
        for (name, models) in providers {
            r.register(
                (*name).to_string(),
                Arc::new(FakeProvider {
                    serves: models.iter().map(|m| (*m).to_string()).collect(),
                    catalog: Some(models.iter().map(|m| (*m).to_string()).collect()),
                    refusal: None,
                }),
            );
        }
        r
    }

    #[tokio::test]
    async fn fake_provider_is_a_minimal_registry_stub() {
        // The resolver only asks the registry `has()`, so the fixture provider
        // is inert; this pins its stub answers so the impl stays measured.
        use leviath_providers::Provider as _;
        let p = FakeProvider::default();
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

    /// The case that reached Ollama as a request for `ollama/qwen3.8:latest`: a
    /// `default_model` written as `provider/model` next to the provider it
    /// names. The prefix is dropped on the way to the resolver.
    #[test]
    fn a_default_model_qualified_with_its_own_provider_is_sent_bare() {
        let defaults = ModelDefaults {
            provider: "ollama".to_string(),
            model: Some("ollama/qwen3.8:latest".to_string()),
            fallback_order: Vec::new(),
        };
        let (p, m) = resolve_stage_model(
            &model_cfg(vec![("ghost", "g")]),
            None,
            &defaults,
            &registry_with(&["ollama"]),
        );
        assert_eq!((p.as_str(), m.as_str()), ("ollama", "qwen3.8:latest"));
    }

    #[test]
    fn bare_default_model_strips_only_the_named_providers_prefix() {
        // The provider's own name, and nothing else, comes off.
        assert_eq!(
            bare_default_model("ollama", "ollama/qwen3.8:latest"),
            "qwen3.8:latest"
        );
        assert_eq!(
            bare_default_model("anthropic", "anthropic/claude-sonnet-5"),
            "claude-sonnet-5"
        );
        // Another provider's name is part of the model id, as far as this
        // provider is concerned.
        assert_eq!(bare_default_model("ollama", "openai/gpt-5"), "openai/gpt-5");
        // A bare id is untouched, as is one that merely starts with the same
        // letters or has nothing after the slash.
        assert_eq!(
            bare_default_model("ollama", "qwen3.8:latest"),
            "qwen3.8:latest"
        );
        assert_eq!(bare_default_model("ollama", "ollamafoo/x"), "ollamafoo/x");
        assert_eq!(bare_default_model("ollama", "ollama/"), "ollama/");
        assert_eq!(bare_default_model("ollama", "ollama"), "ollama");
    }

    #[test]
    fn bare_default_model_keeps_openrouters_own_catalog_ids() {
        // `openrouter/auto` is a real OpenRouter model, not a qualified one.
        assert_eq!(
            bare_default_model("openrouter", "openrouter/auto"),
            "openrouter/auto"
        );
        // A qualified OpenRouter model still carries the vendor segment, so
        // there is a second slash and the prefix comes off.
        assert_eq!(
            bare_default_model("openrouter", "openrouter/deepseek/deepseek-v4-flash"),
            "deepseek/deepseek-v4-flash"
        );
        assert_eq!(
            bare_default_model("openrouter", "deepseek/deepseek-v4-flash"),
            "deepseek/deepseek-v4-flash"
        );
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
            catalog(&tools),
            false,
            None,
        )
        .expect("anthropic is registered");
        assert!(resolved[0].tools.is_empty());
    }

    #[test]
    fn resolve_stages_refuses_a_stage_with_no_usable_provider() {
        // The last fallback in `resolve_stage_model` is unchecked, so "ghost"
        // resolves and would spawn an agent that can never take a turn. It has
        // to be an error the caller sees.
        let stage = leviath_core::Stage::new("plan".to_string(), model_cfg(vec![("ghost", "m")]));
        let layout = leviath_core::layout::ContextLayout::new(vec![], 1000);
        let bp = Blueprint::new("t".to_string(), "d".to_string(), vec![stage], layout);

        let err = resolve_stages(
            &bp,
            None,
            &ModelDefaults::default(),
            &registry_with(&[]),
            catalog(&[]),
            false,
            None,
        )
        .expect_err("no provider is configured");

        assert!(err.contains("plan"), "names the stage: {err}");
        assert!(err.contains("ghost"), "names what it tried: {err}");
        assert!(err.contains("lev setup"), "says what to do: {err}");
    }

    /// A provider that is here and says it does not carry the model.
    ///
    /// Distinct from "no usable provider" above: this provider answers, so
    /// nothing fails over and the run would spend its life posting a model id
    /// the API rejects. Refused rather than skipped on purpose - promoting the
    /// next entry is the silent substitution this check exists to end.
    #[test]
    fn resolve_stages_refuses_a_model_the_provider_does_not_carry() {
        let stage = leviath_core::Stage::new(
            "plan".to_string(),
            model_cfg(vec![("groq", "llama-3.1-70b")]),
        );
        let layout = leviath_core::layout::ContextLayout::new(vec![], 1000);
        let bp = Blueprint::new("t".to_string(), "d".to_string(), vec![stage], layout);

        let err = resolve_stages(
            &bp,
            None,
            &ModelDefaults::default(),
            &registry_publishing(&[("groq", &["llama-4-scout"])]),
            catalog(&[]),
            false,
            None,
        )
        .expect_err("groq published a catalogue and this model is not in it");

        assert!(err.contains("plan"), "names the stage: {err}");
        assert!(err.contains("groq/llama-3.1-70b"), "names the pair: {err}");
        assert!(err.contains("lev models list"), "says what to do: {err}");
    }

    /// And when the provider can say *why*, the spawn error says that instead.
    ///
    /// "Does not serve it" is right for a typo and wrong for a model the route
    /// carries and the account cannot reach: Codex carries
    /// `gpt-5.3-codex-spark`, a Plus plan does not include it, and the two
    /// send a reader to different places - one to check the spelling, the
    /// other to change the stage or the plan. This is the message somebody
    /// actually reads when a run will not start.
    #[test]
    fn resolve_stages_prefers_the_providers_own_reason() {
        let stage = leviath_core::Stage::new(
            "plan".to_string(),
            model_cfg(vec![("codexish", "gpt-5.3-spark")]),
        );
        let layout = leviath_core::layout::ContextLayout::new(vec![], 1000);
        let bp = Blueprint::new("t".to_string(), "d".to_string(), vec![stage], layout);

        let mut registry = ProviderRegistry::new();
        registry.register(
            "codexish".to_string(),
            Arc::new(FakeProvider {
                serves: vec!["gpt-5.5".to_string()],
                catalog: Some(vec!["gpt-5.5".to_string()]),
                refusal: Some(
                    "your ChatGPT plus plan does not include it. Available: gpt-5.5".to_string(),
                ),
            }),
        );

        let err = resolve_stages(
            &bp,
            None,
            &ModelDefaults::default(),
            &registry,
            catalog(&[]),
            false,
            None,
        )
        .expect_err("the plan does not reach it");

        assert!(err.contains("plan"), "names the stage: {err}");
        assert!(
            err.contains("codexish/gpt-5.3-spark"),
            "names the pair: {err}"
        );
        assert!(
            err.contains("plus plan does not include"),
            "the reason: {err}"
        );
        assert!(
            !err.contains("does not serve"),
            "the reason replaces the guess rather than sitting beside it: {err}"
        );
    }

    /// The same shape, against a provider that publishes nothing. A provider
    /// that will not say what it serves cannot refuse anything, so the stage
    /// resolves exactly as it would with no check at all.
    #[test]
    fn resolve_stages_allows_a_model_an_unpublishing_provider_never_denied() {
        let stage = leviath_core::Stage::new(
            "plan".to_string(),
            model_cfg(vec![("groq", "llama-3.1-70b")]),
        );
        let layout = leviath_core::layout::ContextLayout::new(vec![], 1000);
        let bp = Blueprint::new("t".to_string(), "d".to_string(), vec![stage], layout);

        let resolved = resolve_stages(
            &bp,
            None,
            &ModelDefaults::default(),
            &registry_with(&["groq"]),
            catalog(&[]),
            false,
            None,
        )
        .expect("silence is not a refusal");

        assert_eq!(resolved[0].model, "llama-3.1-70b");
    }

    /// A fallback the provider will refuse is dropped rather than kept, so a
    /// failover cannot spend a step landing on it. The head is not filtered
    /// this way - `resolve_stages` refuses it outright instead.
    #[test]
    fn a_fallback_the_provider_refuses_is_dropped_from_the_chain() {
        let cfg = model_cfg(vec![
            ("groq", "llama-4-scout"),
            ("groq", "llama-3.1-70b"),
            ("groq", "llama-4-maverick"),
        ]);
        let registry = registry_publishing(&[("groq", &["llama-4-scout", "llama-4-maverick"])]);

        let picked = resolve_stage_candidates(&cfg, None, &ModelDefaults::default(), &registry);

        let models: Vec<&str> = picked.iter().map(|e| e.model.as_str()).collect();
        assert_eq!(models, ["llama-4-scout", "llama-4-maverick"], "{picked:?}");
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
            catalog(&[]),
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
            catalog(&tools),
            false,
            None,
        )
        .expect("anthropic is registered");
        let selected: Vec<&str> = resolved[0].tools.iter().map(|t| t.name.as_str()).collect();
        // `bash` resolved to `shell`; the unknown MCP name and unlisted
        // `read_file` were both excluded.
        assert_eq!(selected, vec!["shell"]);
    }

    // ── failover candidates ───────────────────────────────────────────────

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
        // failover goes.
        assert_eq!(
            pairs(&got),
            vec![
                ("openrouter", "deepseek"),
                ("anthropic", "sonnet"),
                ("openai", "gpt"),
            ]
        );
    }

    // ─── the unattended cut ──────────────────────────────────────────────────

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
    fn an_entry_with_no_provider_resolves_to_whoever_serves_the_model() {
        // What a blueprint that names models rather than routes produces. The
        // author asks for `gpt-5.5`; which providers can reach it is a property
        // of the machine, so every registered one is asked.
        let cfg = model_cfg_open(vec!["gpt-5.5"]);
        let registry = registry_serving(&[
            ("anthropic", &[][..]),
            ("openai", &["gpt-5.5"][..]),
            ("openrouter", &["gpt-5.5"][..]),
        ]);
        let defaults = ModelDefaults {
            provider: "openrouter".to_string(),
            ..Default::default()
        };
        let got = resolve_stage_candidates(&cfg, None, &defaults, &registry);
        assert_eq!(
            pairs(&got),
            vec![("openrouter", "gpt-5.5"), ("openai", "gpt-5.5")],
            "both routes are offered, the user's default provider first, and \
             the provider that does not serve it is not offered at all"
        );
    }

    #[test]
    fn a_model_nobody_serves_is_skipped_and_the_next_one_is_used() {
        // An unroutable name is not fatal: the chain carries on, which is what
        // a fallback list is for. It warns rather than failing the stage.
        let cfg = model_cfg_open(vec!["nobody-serves-this", "claude-sonnet-5"]);
        let registry = registry_serving(&[("anthropic", &["claude-sonnet-5"][..])]);
        let got = resolve_stage_candidates(&cfg, None, &ModelDefaults::default(), &registry);
        assert_eq!(pairs(&got), vec![("anthropic", "claude-sonnet-5")]);
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

    /// Every bundled blueprint lists Ollama as `qwen3.5:9b`. A user who set
    /// `default_model = "qwen3.8:latest"` was still sent to the blueprint's
    /// model, so the named default leads.
    ///
    /// What it does NOT do is drag the rest of that provider's entries with it.
    /// `default_model` is a statement about a model and moves that model;
    /// `default_provider` is a statement about a route and reorders routes
    /// within a model. So the blueprint's own first model stays ahead of the
    /// blueprint's later ones, and only falls through when nothing registered
    /// can serve it.
    #[test]
    fn the_users_default_model_leads_the_blueprints_entry_on_the_same_provider() {
        let cfg = model_cfg(vec![
            ("anthropic", "claude-sonnet-5"),
            ("ollama", "qwen3.5:9b"),
            ("ollama", "qwen3.6:27b"),
        ]);
        let defaults = ModelDefaults {
            provider: "ollama".to_string(),
            model: Some("qwen3.8:latest".to_string()),
            ..Default::default()
        };
        let registry = registry_with(&["anthropic", "ollama"]);
        let got = resolve_stage_candidates(&cfg, None, &defaults, &registry);
        assert_eq!(
            pairs(&got),
            vec![
                ("ollama", "qwen3.8:latest"),
                ("anthropic", "claude-sonnet-5"),
                ("ollama", "qwen3.5:9b"),
                ("ollama", "qwen3.6:27b"),
            ],
        );

        // A default that repeats the blueprint's own entry moves that model to
        // the head and is listed once. The models behind it keep the
        // blueprint's order rather than the provider's: `default_provider`
        // reorders the routes to a model, not the models themselves.
        let repeated = ModelDefaults {
            provider: "ollama".to_string(),
            model: Some("qwen3.6:27b".to_string()),
            ..Default::default()
        };
        let got = resolve_stage_candidates(&cfg, None, &repeated, &registry);
        assert_eq!(
            pairs(&got),
            vec![
                ("ollama", "qwen3.6:27b"),
                ("anthropic", "claude-sonnet-5"),
                ("ollama", "qwen3.5:9b"),
            ],
        );
    }

    #[test]
    fn the_default_provider_outranks_the_stages_own_list() {
        // Why it has to: the bundled blueprints all name
        // anthropic/openai/ollama, and ollama registers with no key, so an
        // OpenRouter-only install would otherwise dispatch every stage at a
        // localhost server that is not running.
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

    /// A bare `--model` override replaces the model on every entry and leaves
    /// each entry's provider alone, so it invents pairs nobody asked for. One
    /// that no provider serves is dropped rather than left in the fallback
    /// list for a failover to land on a route that cannot answer.
    #[test]
    fn a_rename_onto_a_provider_that_does_not_serve_it_is_dropped() {
        let cfg = model_cfg(vec![("anthropic", "sonnet"), ("openrouter", "gpt")]);
        let registry = registry_serving(&[
            ("anthropic", &["sonnet", "opus"][..]),
            ("openrouter", &["gpt", "gpt-5.5"][..]),
        ]);

        let got =
            resolve_stage_candidates(&cfg, Some("gpt-5.5"), &ModelDefaults::default(), &registry);

        assert_eq!(
            pairs(&got),
            vec![("openrouter", "gpt-5.5")],
            "anthropic does not serve gpt-5.5, so no failover can land there"
        );
    }

    /// The other half of the same rule: silence is not a no. A provider whose
    /// catalogue this build cannot see keeps its pins, because rejecting a pair
    /// nothing can vouch for would throw away every route to such a provider.
    #[test]
    fn a_rename_is_kept_when_no_provider_can_vouch_for_the_model() {
        let cfg = model_cfg(vec![("anthropic", "sonnet"), ("openrouter", "gpt")]);
        // These providers claim nothing at all.
        let registry = registry_with(&["anthropic", "openrouter"]);

        let got = resolve_stage_candidates(
            &cfg,
            Some("some-new-model"),
            &ModelDefaults::default(),
            &registry,
        );

        assert_eq!(
            pairs(&got),
            vec![
                ("anthropic", "some-new-model"),
                ("openrouter", "some-new-model")
            ],
            "nobody could say, so the entries stand as they did before"
        );
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
            catalog(&[]),
            false,
            None,
        )
        .expect("both providers are registered");
        assert_eq!(resolved[0].provider_name, "openrouter");
        assert_eq!(pairs(&resolved[0].fallbacks), vec![("anthropic", "sonnet")]);
    }

    #[test]
    fn an_unattended_run_loses_the_tools_that_wait_on_a_person() {
        // With nobody watching, a call to `ask_user_text` can only park the
        // agent, so the model never sees it.
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
            catalog(&ask_and_read_defs()),
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
            catalog(&submit_tool_defs()),
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
                on_validator_error: None,
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
            catalog(&[Tool {
                name: "read_file".to_string(),
                description: "read a file".to_string(),
                parameters: serde_json::Value::Null,
            }]),
            false,
            None,
        )
        .expect("anthropic is registered");
        assert_eq!(resolved[0].tools[0].description, "read a file");
    }

    fn owners(pairs: &[(&str, &str)]) -> ToolOwners {
        pairs
            .iter()
            .map(|(tool, server)| (tool.to_string(), server.to_string()))
            .collect()
    }

    fn granted_names(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    /// The point of the feature: a stage names the server, and every tool that
    /// server advertises is granted - including ones added after the manifest
    /// was written, which is what naming them individually could never keep up
    /// with.
    #[test]
    fn a_connector_grant_covers_every_tool_its_server_advertises() {
        let owned = owners(&[
            ("create_issue", "github"),
            ("list_prs", "github"),
            ("query", "database"),
        ]);
        let granted =
            expand_connector_grants(&["read_file".to_string()], &["github".to_string()], &owned);
        assert_eq!(
            granted_names(&granted),
            vec!["read_file", "create_issue", "list_prs"],
            "the stage's own names first, then the connector's, and nothing \
             belonging to a server it did not name"
        );
    }

    /// Sorted within a connector, so the set the model is offered does not
    /// reshuffle between runs of the same blueprint - a difference nobody could
    /// explain and every prompt cache would notice.
    #[test]
    fn a_connectors_tools_are_granted_in_a_stable_order() {
        let owned = owners(&[("zeta", "srv"), ("alpha", "srv"), ("mid", "srv")]);
        let first = expand_connector_grants(&[], &["srv".to_string()], &owned);
        assert_eq!(granted_names(&first), vec!["alpha", "mid", "zeta"]);
        // Re-expanding the same inputs gives the same answer, whatever the map
        // iterates in.
        for _ in 0..8 {
            assert_eq!(
                expand_connector_grants(&[], &["srv".to_string()], &owned),
                first
            );
        }
    }

    /// A tool named individually and also covered by a connector is granted
    /// once. Duplicates would reach the provider as a repeated tool definition.
    #[test]
    fn a_tool_named_twice_is_granted_once() {
        let owned = owners(&[("create_issue", "github")]);
        let granted = expand_connector_grants(
            &["create_issue".to_string()],
            &["github".to_string()],
            &owned,
        );
        assert_eq!(granted_names(&granted), vec!["create_issue"]);
    }

    /// A connector that resolves to nothing - server not installed, or not
    /// connected this run - contributes nothing, exactly as an `available_tools`
    /// name matching nothing does. Silence is right here: whether a server is
    /// present is not a property of the manifest.
    #[test]
    fn an_unresolvable_connector_grants_nothing_and_keeps_the_rest() {
        let granted = expand_connector_grants(
            &["read_file".to_string()],
            &["not_installed".to_string()],
            &ToolOwners::new(),
        );
        assert_eq!(granted_names(&granted), vec!["read_file"]);
    }

    /// A stage that grants no connector is untouched, which is every stage in
    /// every blueprint written before this existed.
    #[test]
    fn no_connectors_means_the_list_is_returned_as_written() {
        let owned = owners(&[("create_issue", "github")]);
        let available = vec!["read_file".to_string(), "write_file".to_string()];
        assert_eq!(expand_connector_grants(&available, &[], &owned), available);
    }

    /// End to end through stage resolution: the connector's tools are actually
    /// advertised to the model, and a tool from an unnamed server is not.
    #[test]
    fn a_stage_granting_a_connector_is_offered_its_tools() {
        let defs: Vec<Tool> = ["create_issue", "list_prs", "query"]
            .iter()
            .map(|n| Tool {
                name: n.to_string(),
                description: String::new(),
                parameters: serde_json::Value::Null,
            })
            .collect();
        let owned = owners(&[
            ("create_issue", "github"),
            ("list_prs", "github"),
            ("query", "database"),
        ]);
        let mut stage =
            leviath_core::Stage::new("work".to_string(), model_cfg(vec![("anthropic", "m")]));
        stage.available_tools = vec![];
        stage.available_connectors = vec!["github".to_string()];
        let bp = Blueprint::new(
            "t".to_string(),
            "d".to_string(),
            vec![stage],
            leviath_core::layout::ContextLayout::new(vec![], 1000),
        );

        let resolved = resolve_stages(
            &bp,
            None,
            &ModelDefaults::default(),
            &registry_with(&["anthropic"]),
            ToolCatalog {
                defs: &defs,
                owners: &owned,
            },
            false,
            None,
        )
        .expect("anthropic is registered");

        let offered: Vec<&str> = resolved[0].tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(offered, vec!["create_issue", "list_prs"]);
    }

    /// Two servers advertising the same tool name, which is the case worth
    /// pinning: `leviath-mcp` gives the first registrant the bare name and
    /// prefixes the second, so the advertised names never collide - and a
    /// grant of either kind has to reach exactly one server's tool.
    #[test]
    fn same_named_tools_on_two_servers_stay_separable() {
        // What `unique_advertised_name` produces for two servers that both
        // advertise `search`: alpha registered first and kept the bare name.
        let owned = owners(&[
            ("search", "alpha"),
            ("beta__search", "beta"),
            ("only_beta", "beta"),
        ]);

        // Naming a tool individually reaches one server's, not both.
        let just_beta = expand_connector_grants(&["beta__search".to_string()], &[], &owned);
        assert_eq!(granted_names(&just_beta), vec!["beta__search"]);

        let just_alpha = expand_connector_grants(&["search".to_string()], &[], &owned);
        assert_eq!(granted_names(&just_alpha), vec!["search"]);

        // A connector grant takes that server's tools and none of the other's,
        // even though one of them is named identically on the far side.
        let all_beta = expand_connector_grants(&[], &["beta".to_string()], &owned);
        assert_eq!(granted_names(&all_beta), vec!["beta__search", "only_beta"]);
        let all_alpha = expand_connector_grants(&[], &["alpha".to_string()], &owned);
        assert_eq!(granted_names(&all_alpha), vec!["search"]);

        // And the two mix: a whole connector plus one tool from the other.
        let mixed = expand_connector_grants(&["search".to_string()], &["beta".to_string()], &owned);
        assert_eq!(
            granted_names(&mixed),
            vec!["search", "beta__search", "only_beta"]
        );
    }

    /// Granting two connectors at once keeps both servers' sets whole, with no
    /// entry lost to the other and none duplicated.
    #[test]
    fn granting_two_connectors_keeps_both_sets_whole() {
        let owned = owners(&[
            ("search", "alpha"),
            ("only_alpha", "alpha"),
            ("beta__search", "beta"),
            ("only_beta", "beta"),
        ]);
        let granted =
            expand_connector_grants(&[], &["alpha".to_string(), "beta".to_string()], &owned);
        assert_eq!(
            granted_names(&granted),
            vec!["only_alpha", "search", "beta__search", "only_beta"],
            "each server's tools, sorted within the server, in the order named"
        );
    }

    /// The whole point of separability: what the model is actually offered.
    /// A stage granting only beta must not be handed alpha's `search`, even
    /// though alpha's tool is the one wearing the plain name.
    #[test]
    fn a_stage_granting_one_of_two_colliding_servers_is_offered_only_its_tools() {
        let defs: Vec<Tool> = ["search", "beta__search", "only_beta"]
            .iter()
            .map(|n| Tool {
                name: n.to_string(),
                description: String::new(),
                parameters: serde_json::Value::Null,
            })
            .collect();
        let owned = owners(&[
            ("search", "alpha"),
            ("beta__search", "beta"),
            ("only_beta", "beta"),
        ]);
        let mut stage =
            leviath_core::Stage::new("work".to_string(), model_cfg(vec![("anthropic", "m")]));
        stage.available_tools = vec![];
        stage.available_connectors = vec!["beta".to_string()];
        let bp = Blueprint::new(
            "t".to_string(),
            "d".to_string(),
            vec![stage],
            leviath_core::layout::ContextLayout::new(vec![], 1000),
        );

        let resolved = resolve_stages(
            &bp,
            None,
            &ModelDefaults::default(),
            &registry_with(&["anthropic"]),
            ToolCatalog {
                defs: &defs,
                owners: &owned,
            },
            false,
            None,
        )
        .expect("anthropic is registered");

        let offered: Vec<&str> = resolved[0].tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(offered, vec!["beta__search", "only_beta"]);
    }
    fn defs_named(names: &[&str]) -> Vec<Tool> {
        names
            .iter()
            .map(|n| Tool {
                name: n.to_string(),
                description: String::new(),
                parameters: serde_json::Value::Null,
            })
            .collect()
    }

    fn offered(tools: &[Tool]) -> Vec<&str> {
        tools.iter().map(|t| t.name.as_str()).collect()
    }

    /// A blueprint written before MCP names carried their server keeps working
    /// when the answer is unambiguous. Breaking it would drop the tool
    /// silently, which is the failure the connector work exists to fix.
    #[test]
    fn a_grant_naming_an_mcp_tool_without_its_server_still_resolves() {
        let defs = defs_named(&["github__create_issue", "read_file"]);
        let tools = filter_tools_by_available(&defs, &["create_issue".to_string()]);
        assert_eq!(offered(&tools), vec!["github__create_issue"]);
    }

    /// Two servers offering it means the name really is ambiguous, and there is
    /// nothing to choose between them. It resolves to no tool rather than to
    /// one of them: a silent "whichever registered first" is what qualified
    /// names exist to prevent.
    #[test]
    fn an_ambiguous_unqualified_grant_resolves_to_nothing() {
        let defs = defs_named(&["github__create_issue", "gitlab__create_issue"]);
        let tools = filter_tools_by_available(&defs, &["create_issue".to_string()]);
        assert_eq!(offered(&tools), Vec::<&str>::new());
    }

    /// The case that matters most, because getting it wrong would break a
    /// working stage rather than fail to fix a broken one: a built-in matches
    /// itself, so a server offering a tool of the same name cannot capture the
    /// grant. The fallback only ever sees names that matched nothing.
    #[test]
    fn a_builtin_is_never_captured_by_a_server_offering_the_same_name() {
        let defs = defs_named(&["read_file", "scratch__read_file"]);
        let tools = filter_tools_by_available(&defs, &["read_file".to_string()]);
        assert_eq!(
            offered(&tools),
            vec!["read_file"],
            "the built-in, not the server's tool of the same name"
        );
    }

    /// A name that already matches an advertised tool is never rewritten, so
    /// the fallback cannot redirect a grant that was correct to begin with -
    /// even when another tool ends the same way.
    #[test]
    fn an_already_qualified_grant_is_untouched() {
        let defs = defs_named(&["github__search", "other__github__search"]);
        let tools = filter_tools_by_available(&defs, &["github__search".to_string()]);
        assert_eq!(offered(&tools), vec!["github__search"]);
    }

    /// A name matching nothing at all is still simply omitted, as it always
    /// was: a typo, or an MCP server that is not installed.
    #[test]
    fn a_name_matching_nothing_is_still_omitted() {
        let defs = defs_named(&["read_file"]);
        let tools = filter_tools_by_available(&defs, &["nonsense".to_string()]);
        assert!(offered(&tools).is_empty());
    }

    // ── explicit-route-only providers ───────────────────────────────────────

    /// A provider serving one model, optionally only reachable by name.
    struct Named {
        model: String,
        by_name_only: bool,
    }

    #[async_trait::async_trait]
    impl leviath_providers::Provider for Named {
        async fn infer(
            &self,
            _: &leviath_providers::InferenceRequest,
        ) -> leviath_providers::Result<leviath_providers::InferenceResponse> {
            Err(leviath_providers::ProviderError::Other("stub".to_string()))
        }
        async fn count_tokens(&self, _: &str, _: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _: &str) -> usize {
            100_000
        }
        fn name(&self) -> &str {
            "named"
        }
        fn capabilities(&self, _: &str) -> leviath_providers::ModelCapabilities {
            leviath_providers::ModelCapabilities::default()
        }
        fn serves_model(&self, model_key: &str) -> Option<String> {
            (model_key == self.model).then(|| model_key.to_string())
        }
        fn explicit_route_only(&self) -> bool {
            self.by_name_only
        }
    }

    fn two_providers(open_first: bool) -> ProviderRegistry {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "openai".to_string(),
            Arc::new(Named {
                model: "gpt-5.6-sol".to_string(),
                by_name_only: !open_first,
            }),
        );
        registry.register(
            "codex".to_string(),
            Arc::new(Named {
                model: "gpt-5.6-sol".to_string(),
                by_name_only: true,
            }),
        );
        registry
    }

    fn defaults_of(provider: &str) -> ModelDefaults {
        ModelDefaults {
            provider: provider.to_string(),
            model: None,
            fallback_order: Vec::new(),
        }
    }

    /// The stub answers every method, so none of its bodies is scenery. A
    /// resolution never infers, but leaving that arm unreachable would be an
    /// uncovered region like any other.
    #[test]
    fn the_named_stub_answers_the_whole_trait() {
        use leviath_providers::Provider as _;
        let p = Named {
            model: "m".to_string(),
            by_name_only: false,
        };
        assert_eq!(p.name(), "named");
        assert_eq!(p.max_context_tokens("m"), 100_000);
        assert_eq!(
            p.capabilities("m"),
            leviath_providers::ModelCapabilities::default()
        );
        assert!(p.serves_model("m").is_some());
        assert!(p.serves_model("other").is_none());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime");
        assert_eq!(runtime.block_on(p.count_tokens("t", "m")), 1);
        let request = leviath_providers::InferenceRequest {
            system: Vec::new(),
            messages: Vec::new(),
            model: "m".to_string(),
            max_tokens: 1,
            temperature: 0.0,
            tools: Vec::new(),
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        assert!(runtime.block_on(p.infer(&request)).is_err());
    }

    /// Enabling a subscription transport must not silently move billing.
    ///
    /// `model_key` compares only the last path segment, so `openai` and `codex`
    /// both answer to a bare `gpt-5.6-sol`. Without the exclusion, adding one
    /// line of config re-routes every existing bare-named stage onto a plan.
    #[test]
    fn a_by_name_only_provider_never_wins_an_open_route() {
        let picked = resolve_stage_candidates(
            &model_cfg_open(vec!["gpt-5.6-sol"]),
            None,
            &defaults_of("openai"),
            &two_providers(true),
        );
        assert_eq!(picked.len(), 1, "codex joined an open route: {picked:?}");
        assert_eq!(picked[0].provider, "openai");
    }

    /// Choosing it as the default is asking for it, so the exclusion lifts.
    #[test]
    fn naming_it_as_the_default_is_asking_for_it() {
        let picked = resolve_stage_candidates(
            &model_cfg_open(vec!["gpt-5.6-sol"]),
            None,
            &defaults_of("codex"),
            &two_providers(true),
        );
        assert_eq!(picked[0].provider, "codex");
    }

    /// So is naming it in the blueprint.
    #[test]
    fn an_explicit_provider_reaches_it() {
        let picked = resolve_stage_candidates(
            &model_cfg(vec![("codex", "gpt-5.6-sol")]),
            None,
            &defaults_of("openai"),
            &two_providers(true),
        );
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].provider, "codex");
    }

    /// And so is a `--model codex/...` override.
    #[test]
    fn a_model_override_reaches_it() {
        let picked = resolve_stage_candidates(
            &model_cfg_open(vec!["gpt-5.6-sol"]),
            Some("codex/gpt-5.6-sol"),
            &defaults_of("openai"),
            &two_providers(true),
        );
        assert_eq!(picked[0].provider, "codex");
    }

    /// With nothing else serving the name, an open route finds nobody rather
    /// than falling through to the excluded provider. Silently spending a
    /// subscription is the outcome the exclusion exists to prevent, so it has
    /// to hold even when the alternative is no route at all.
    #[test]
    fn the_exclusion_holds_when_nothing_else_serves_the_name() {
        // Neither provider is the default here, so nothing is exempt. The open
        // route finds no one, and what is left is the unrouted entry the
        // user-default path always appends - not a fall-through onto a
        // provider that would have spent a subscription.
        let picked = resolve_stage_candidates(
            &model_cfg_open(vec!["gpt-5.6-sol"]),
            None,
            &defaults_of("anthropic"),
            &two_providers(false),
        );
        assert!(
            picked.iter().all(|e| e.provider != "codex"),
            "a by-name-only provider was reached anyway: {picked:?}"
        );
        assert!(
            picked.iter().all(|e| e.provider != "openai"),
            "got {picked:?}"
        );
    }
}
