//! Blueprint lint: the checks a graph's own validation deliberately does not
//! make.
//!
//! [`RunGraph::validate`] answers "does this graph hold together": every
//! stage, region and input it names is declared, every setting is in range.
//! It says nothing about the fields whose *absence* quietly changes what a run
//! does, and those are what actually bite:
//!
//! - a stage with an empty model list runs on whatever the user's default
//!   provider happens to be
//! - a typo in `tools` matches nothing, and the stage just advertises one tool
//!   fewer, so the model is told the tool does not exist
//! - an autonomous stage granting `ask_user_text` parks in `WaitingInput` the
//!   first time it asks, with nobody there to answer
//!
//! Each of those is invisible on inspection and shows up hours later as a stuck
//! run. This module names them at author time instead.
//!
//! [`RunGraph::validate`]: leviath_runtime::spec::graph::RunGraph::validate

use std::collections::{HashMap, HashSet};
use std::path::Path;

use leviath_blueprint::BlueprintFile;
use leviath_runtime::dynamic_interaction::BLOCKING_INTERACTION_TOOLS;
use leviath_runtime::spec::graph::{RunGraph, StageMode, ToolGroup, WorkerSource};
use leviath_tools::canonical_tool_name;
// The findings every check reports in belong to the blueprint layer, so the
// daemon's spawn log, `lev validate` and the blueprint editor show them alike.
pub(crate) use leviath_blueprint::lint::{LintFinding, LintSeverity};

/// What one provider answered when asked what models it takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderCatalog {
    /// It named everything it carries. A model outside this list is a model it
    /// will refuse, so naming one is a fault in the blueprint rather than a
    /// fact about the machine.
    Complete(Vec<String>),

    /// It is reachable but would not say what it carries, and it is a script
    /// provider - one with neither a `list_models` function nor a
    /// `[model_providers.<name>] serves` list.
    ///
    /// Only script providers are recorded this way. Every built-in whose
    /// catalogue this build cannot see is either covered by the compiled-in
    /// table that `unknown-model` reads (Anthropic, OpenAI, Google) or has an
    /// open catalogue where silence is the correct answer (Ollama serves
    /// whatever has been pulled). A script provider is the one case where the
    /// silence is the author's to fix, and where saying nothing is what let a
    /// wrong model id reach a run unremarked.
    ScriptSaidNothing,
}

/// Facts about the machine the blueprint will run on, which the blueprint alone
/// cannot supply.
///
/// Every field is "unknown" when empty/`None`, and an unknown field skips its
/// check entirely rather than guessing. A linter that cannot see the installed
/// MCP servers must not claim their tools do not exist.
#[derive(Debug, Default, Clone)]
pub(crate) struct LintEnv {
    /// Every tool name a blueprint may legally write: canonical built-ins, their
    /// aliases, the sub-agent tools, this agent's own `tools/*.rhai`, and any
    /// MCP tools already resolved. Empty skips the unknown-tool check.
    pub known_tools: HashSet<String>,

    /// Which group each known tool belongs to, so a check can say whether a
    /// `@builtin`-style grant reaches a tool named elsewhere in the stage
    /// (`required_tools`, `tool_permissions`). MCP tools are absent: their
    /// `server__tool` shape already places them in [`ToolGroup::Mcp`]. Empty
    /// means the question was never asked, and no group-aware check guesses.
    pub tool_sources: HashMap<String, ToolGroup>,

    /// `(provider, model)` rows for providers whose catalog is closed enough to
    /// check against. A provider with no row here is not checked at all, which
    /// is what keeps open catalogs (Ollama, OpenRouter, script providers) from
    /// producing noise.
    pub known_models: Vec<(String, String)>,

    /// The providers the blueprint names that this install can actually reach,
    /// as answered by `ProviderRegistry::has`. `None` means nobody asked, so
    /// the check is skipped. Resolution lives with the caller because script
    /// providers are loaded on demand and cannot be enumerated up front.
    pub available_providers: Option<HashSet<String>>,

    /// Which of the blueprint's `read_paths` this install's config grants.
    /// `None` means nobody asked (the daemon's offline lint), in which case the
    /// check only says that a declaration needs granting. `Some(Err(..))` is a
    /// grant list of the user's own that will not compile.
    pub read_paths: Option<Result<crate::read_path_report::GrantReport, String>>,

    /// Whether this install's config honours the blueprint's own
    /// `[graph.safe_commands]`. `None` means nobody asked (the daemon's offline
    /// lint), in which case the check only says the declaration needs granting.
    ///
    /// A bool rather than a report: unlike read paths, where *which* entries are
    /// granted is the interesting part, a safe-commands block is honoured whole
    /// or not at all.
    pub safe_commands_granted: Option<bool>,

    /// What each provider the blueprint names says it serves, read off the same
    /// primed registry the runtime resolves against.
    ///
    /// A provider absent from this map was not asked - either nobody built a
    /// registry (the daemon's offline lint) or this install cannot reach it -
    /// and its entries go unchecked. See [`ProviderCatalog`] for what the two
    /// present states mean.
    pub provider_catalogs: HashMap<String, ProviderCatalog>,
    /// Why a provider refuses one particular `provider/model`, when it had
    /// more to say than the absence itself - keyed the way a blueprint writes
    /// it.
    ///
    /// A catalogue that depends on the *account* rather than the build makes
    /// "does not serve it" wrong: Codex carries `gpt-5.3-codex-spark` and a
    /// Plus plan cannot reach it, and telling somebody to check their
    /// spelling sends them nowhere useful.
    pub provider_refusals: HashMap<String, String>,

    /// Bare model names the blueprint leaves unrouted: it names the model and
    /// no provider, and nothing this install can reach claims to serve it.
    ///
    /// Asked of the registry exactly as [`resolve_stage_candidates`] asks it,
    /// so this is the set of entries resolution silently drops. It is what
    /// lets `no-reachable-provider` judge an open entry at all: without it that
    /// check had to assume every open entry was fine, and so skipped itself on
    /// every blueprint written in the form they are all written in.
    ///
    /// Empty when nobody asked, which is indistinguishable from "everything
    /// routes". Both mean the same thing to the check that reads it - treat the
    /// entry as reachable - so an unasked question cannot become a finding.
    ///
    /// [`resolve_stage_candidates`]: leviath_runtime::pipeline::resolve_stage_candidates
    pub unrouted_models: HashSet<String>,

    /// Context-window size per `(provider, model)`, for the models this build
    /// ships a capability row for.
    ///
    /// Only needed to say what a percentage budget *resolves to*: "38%" is not
    /// alarming until you know the denominator is a million. Empty skips the
    /// unbounded-percentage check, because a warning that cannot name a number
    /// is a warning nobody acts on.
    pub model_windows: HashMap<(String, String), usize>,

    /// Under `[providers] zero_retention`, the stages whose models keep
    /// something: keyed by stage name, each entry the `provider/model` rows
    /// that would be refused at spawn (the head) or dropped from failover
    /// (a fallback), with the provider's reason. Asked of the same primed
    /// registry the spawn gate asks, with the same settings. Empty when
    /// nobody asked or the switch is off.
    pub retention_refusals: HashMap<String, Vec<RetentionRefusal>>,

    /// Each fan-out worker blueprint the graph names that this install cannot
    /// load, keyed as the graph writes it, with why. `None` means nobody asked
    /// (the daemon's offline lint, whose spawn makes the same check itself).
    pub unloadable_workers: Option<HashMap<String, String>>,
}

/// One model a stage names that cannot run with zero data retention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetentionRefusal {
    /// `provider/model`, as the resolver would run it.
    pub route: String,
    /// Whether this is the model the stage would start on (refused at
    /// spawn) or a fallback (dropped from failover).
    pub head: bool,
    /// What is kept and why, in the provider's words.
    pub reason: String,
}

impl LintEnv {
    /// Everything that can be known without touching the user's config: the
    /// built-in tools (aliases included), the sub-agent tools, the script tools
    /// in `agent_dir/tools` and the global tools directory, and the model
    /// catalogs this build ships.
    ///
    /// This is what the daemon lints against at spawn. It deliberately leaves
    /// `available_providers` unset: the daemon already fails a spawn outright
    /// when no listed provider is registered, so re-deriving that here would
    /// cost a registry build per agent to say something the spawn will say
    /// louder a moment later.
    pub(crate) fn offline(agent_dir: &Path) -> Self {
        // The four discovery rules live in `tool_inventory` rather than here,
        // because `GET /api/tools` has to answer the same question and two
        // copies of "where does a tool come from" would not have stayed equal.
        // The lint wants only the names; the endpoint wants the sources too.
        let inventory = crate::tool_inventory::ToolInventory::discover(Some(agent_dir), None);
        let known_tools = inventory.names();
        let tool_sources = inventory
            .tools
            .iter()
            .map(|t| (t.name.clone(), t.source.group()))
            .collect();

        Self {
            known_tools,
            tool_sources,
            known_models: crate::commands::models::closed_catalog_models(),
            available_providers: None,
            read_paths: None,
            safe_commands_granted: None,
            // Both empty for the same reason `available_providers` is `None`:
            // reading a provider's catalogue means building and priming a
            // registry, which is a network call per provider, and the daemon
            // already refuses a spawn that names a model its provider will not
            // serve. Doing it again here would cost every agent a round of
            // priming to say something the spawn says louder a moment later.
            provider_catalogs: HashMap::new(),
            provider_refusals: HashMap::new(),
            unrouted_models: HashSet::new(),
            model_windows: crate::commands::models::builtin_model_windows(),
            retention_refusals: HashMap::new(),
            unloadable_workers: None,
        }
    }

    /// Add which fan-out worker blueprints the graph names that cannot be
    /// loaded from `agents_dir`, asked exactly as a spawn asks, so `lev
    /// validate` refuses what the spawn would.
    pub(crate) fn with_workers(mut self, graph: &RunGraph, agents_dir: Option<&Path>) -> Self {
        let unloadable = graph
            .stages
            .iter()
            .filter_map(|stage| match &stage.mode {
                StageMode::FanOut(fan) => match &fan.worker {
                    WorkerSource::Blueprint(reference) => Some(reference),
                    _ => None,
                },
                _ => None,
            })
            .filter_map(|reference| {
                crate::daemon::resolve_env::load_installed(agents_dir, reference)
                    .err()
                    .map(|issue| (reference.to_string(), issue.message))
            })
            .collect();
        self.unloadable_workers = Some(unloadable);
        self
    }

    /// Add, under `[providers] zero_retention`, which of each stage's models
    /// cannot run with zero data retention: the one the stage would start
    /// on, which the spawn gate refuses, and the fallbacks, which it drops.
    /// Asked of the primed registry with the config's settings, so the
    /// answer is the one a spawn would get (a Bedrock model the listing
    /// never offers under mode `none`, an OpenRouter model with no
    /// zero-retention endpoint, a provider whose agreement is not declared).
    /// Nothing is recorded when the switch is off.
    pub(crate) fn with_retention(
        mut self,
        graph: &RunGraph,
        config: &crate::config::Config,
        registry: &leviath_runtime::ProviderRegistry,
    ) -> Self {
        let defaults = crate::daemon::spawn::model_defaults(config);
        if !defaults.retention.zero_requested {
            return self;
        }
        // The head is chosen with the switch off: with it on, the choice
        // itself refuses a model that keeps something, and the point here is
        // to name that model rather than to be refused by it.
        let mut choosing = defaults.clone();
        choosing.retention.zero_requested = false;
        for stage in &graph.stages {
            let Ok(head) =
                leviath_runtime::bind::host::choose_model(stage, None, &choosing, registry)
            else {
                continue;
            };
            let mut refusals = Vec::new();
            let mut seen = HashSet::new();
            let mut consider = |provider: &str, model: &str, head: bool| {
                let route = format!("{provider}/{model}");
                if !seen.insert(route.clone()) {
                    return;
                }
                let policy = registry.retention_with(&defaults.retention, provider, model);
                if !policy.is_zero() {
                    refusals.push(RetentionRefusal {
                        route,
                        head,
                        reason: format!(
                            "retention {}: {}",
                            policy.retention.describe(),
                            policy.note
                        ),
                    });
                }
            };
            consider(head.model.provider.as_str(), head.model.id.as_str(), true);
            // The pinned entries the stage names after its head, on providers
            // this install has, which are what a failover would reach. An open
            // entry resolves through the same preference the head did, and is
            // judged there; a provider that is not configured here is not in
            // the failover list either.
            for (provider, model) in stage
                .model
                .models
                .iter()
                .map(route)
                .filter(|(p, _)| !p.is_empty() && registry.has(p))
            {
                consider(provider, model, false);
            }
            if !refusals.is_empty() {
                self.retention_refusals
                    .insert(stage.name.to_string(), refusals);
            }
        }
        self
    }

    /// Add the answer to "can this install reach the providers the blueprint
    /// names", asked of the same registry the runtime resolves stages against
    /// so a script provider counts exactly when it would really load.
    pub(crate) fn with_providers(
        mut self,
        graph: &RunGraph,
        config: &crate::config::Config,
    ) -> Self {
        let registry = crate::commands::run::build_provider_registry_from_config(config);
        self.available_providers = Some(
            model_entries(graph)
                .map(|e| route(e).0.to_string())
                .filter(|p| registry.as_ref().is_ok_and(|r| r.has(p)))
                .collect(),
        );
        self
    }

    /// Add what each provider the blueprint pins says it serves, and which of
    /// the blueprint's unpinned model names nothing here routes.
    ///
    /// Takes the registry the caller already built and primed rather than
    /// building another. `with_providers` builds its own because it only needs
    /// a name lookup; this needs the *primed* one, since an unprimed provider
    /// has no catalogue to report and would turn every check below into a
    /// shrug. `lev validate` primes once and hands the same registry here.
    pub(crate) fn with_provider_catalogs(
        mut self,
        graph: &RunGraph,
        config: &crate::config::Config,
        registry: &leviath_runtime::ProviderRegistry,
    ) -> Self {
        use leviath_runtime::pipeline::model_key;

        for name in model_entries(graph)
            .map(|e| route(e).0)
            .filter(|p| !p.is_empty())
        {
            if self.provider_catalogs.contains_key(name) {
                continue;
            }
            // Not reachable here is not the same question, and
            // `no-reachable-provider` already answers it. Leaving the name out
            // of the map is what keeps a machine that simply lacks a provider
            // from being told its blueprint is wrong.
            let Some(provider) = registry.get(name) else {
                continue;
            };
            let catalog = match provider.served_catalog() {
                Some(models) => ProviderCatalog::Complete(models),
                // Only a script provider's silence is worth reporting: see
                // `ProviderCatalog::ScriptSaidNothing`. `script_provider_named`
                // answers `None` for a native provider of the same name, which
                // is exactly the distinction wanted.
                None if registry.script_provider_named(name).is_some() => {
                    ProviderCatalog::ScriptSaidNothing
                }
                None => continue,
            };
            self.provider_catalogs.insert(name.to_string(), catalog);
        }

        // And what each refused entry's provider says about it, asked while
        // the provider is still in hand: the checks run over plain data.
        //
        // No "already asked" guard, unlike the loop above: that one is keyed
        // by provider and its question can cost a network call, while this is
        // keyed by the exact pair and answered from memory.
        for (provider_name, model) in model_entries(graph).map(route) {
            let Some(provider) = registry.get(provider_name) else {
                continue;
            };
            if let Some(reason) = provider.refusal_reason(model_key(model)) {
                self.provider_refusals
                    .insert(format!("{provider_name}/{model}"), reason);
            }
        }

        // The open entries, asked the way the resolver asks: does a provider
        // in the preference claim this model. A provider outside the
        // preference never serves a bare name, so it is not asked. A script
        // provider is not in `native_providers`, so the machine's default is
        // offered the question too, matching the resolver.
        let defaults = crate::daemon::spawn::model_defaults(config);
        let default_script = registry.script_provider_named(&config.default_provider);
        for (_, model) in model_entries(graph)
            .map(route)
            .filter(|(p, _)| p.is_empty())
        {
            let key = model_key(model);
            let routed = registry
                .native_providers()
                .iter()
                .any(|(name, p)| defaults.is_preferred(name) && p.serves_model(key).is_some())
                || default_script
                    .as_ref()
                    .is_some_and(|p| p.serves_model(key).is_some());
            if !routed {
                self.unrouted_models.insert(model.to_string());
            }
        }
        self
    }

    /// Add the answer to "does this install's config grant what the blueprint
    /// declares under `read_paths`", per entry, for the agent named `agent`.
    ///
    /// Separate from [`Self::with_providers`] because it needs a workdir:
    /// relative entries resolve against the one a run would use, which for a
    /// command run outside a run is the directory it was invoked from.
    pub(crate) fn with_read_paths(
        mut self,
        graph: &RunGraph,
        agent: &str,
        config: &crate::config::Config,
        workdir: &Path,
    ) -> Self {
        self.read_paths = crate::read_path_report::build(graph, agent, config, workdir);
        // Asked here rather than in its own builder: both answers come from the
        // same config, and a caller that has one always has the other.
        self.safe_commands_granted = Some(
            config.security.allow_blueprint_safe_commands
                || config
                    .agent_safe_commands
                    .get(agent)
                    .is_some_and(|a| a.allow_blueprint),
        );
        self
    }
}

/// Lint the blueprint in `file`.
pub(crate) fn lint_blueprint(file: &BlueprintFile, env: &LintEnv) -> Vec<LintFinding> {
    let graph = file.run_graph();
    let graph = &graph;
    let mut findings = Vec::new();

    findings.extend(lint_command_seeds(graph));
    findings.extend(lint_tool_seeds(graph));
    findings.extend(lint_read_paths(graph, env));
    findings.extend(lint_safe_commands(graph, file.blueprint.name.as_str(), env));
    findings.extend(lint_held_checkpoints(graph));
    findings.extend(lint_graph(graph));
    findings.extend(lint_output_reachable(graph));
    findings.extend(lint_dead_end_possible(graph));
    findings.extend(lint_compacted_deliverables(graph));
    findings.extend(lint_required_regions_enforceable(graph));
    findings.extend(lint_unbounded_percentage(graph, env));
    findings.extend(lint_long_context_price(graph, env));
    findings.extend(lint_mime_types(graph));

    for stage in &graph.stages {
        findings.extend(lint_declarations(stage));
        findings.extend(lint_tools(stage, env));
        findings.extend(lint_blocking_tools(stage));
        findings.extend(lint_tool_policies(stage, &graph.tool_permissions));
        findings.extend(lint_permission_clamp(stage, &graph.tool_permissions));
        findings.extend(lint_models(stage, env));
        findings.extend(lint_retention(stage, env));
        findings.extend(lint_output_stage(stage));
        findings.extend(lint_output_stage_can_answer(stage));
        findings.extend(lint_fanout_escape(graph, stage));
        findings.extend(lint_fanout_worker_task(graph, stage));
        findings.extend(lint_fanout_worker_loads(stage, env));
        findings.extend(lint_stage_mime(graph, stage));
        findings.extend(lint_tool_accepts(stage));
    }

    // Worst first, stable within a severity so the order a check ran in is the
    // order its findings read in.
    findings.sort_by_key(|f| f.severity);
    findings
}

// The checks themselves, one module per question they answer. Imported rather
// than re-exported: `lint_blueprint` is the only caller and the only entry
// point anyone outside this module needs, so the individual checks stay
// internal.
mod checks;
mod fanout;
mod graph;
mod mime;
mod pricing;
mod security;
use checks::*;
use fanout::*;
use graph::*;
use mime::*;
use pricing::*;
use security::*;

#[cfg(test)]
mod tests;
