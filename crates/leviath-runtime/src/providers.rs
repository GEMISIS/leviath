//! The [`ProviderRegistry`]: a name → [`Provider`] lookup shared by the ECS
//! pipeline (as the `Providers` resource) and the CLI/daemon spawn path.

use crate::script_provider::ScriptProviderLayer;
use leviath_providers::Provider;
use std::collections::HashMap;
use std::sync::Arc;

/// Registry of inference providers, keyed by provider name (e.g. `"anthropic"`).
///
/// The pipeline resolves each agent's stage `ModelConfig` to a concrete
/// provider through this registry. Native providers are registered eagerly;
/// script providers are resolved lazily - and hot-reloaded - via
/// an optional [`ScriptProviderLayer`].
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn Provider>>,
    /// Providers a previous registry had that this one was built without: a
    /// key the user removed with `lev setup`, an endpoint entry deleted from
    /// `config.toml`. A run already mid-stage on one keeps calling it through
    /// [`get`](Self::get), so its stage finishes on the provider it started
    /// on; nothing new resolves to it, because [`has`](Self::has),
    /// [`native_providers`](Self::native_providers) and
    /// [`resolvable_names`](Self::resolvable_names) leave it out.
    retired: HashMap<String, Arc<dyn Provider>>,
    /// Lazy, hot-reloading resolver for `.rhai` script providers. Shared across
    /// registry clones (one compile cache daemon-wide).
    script_layer: Option<Arc<ScriptProviderLayer>>,
    /// The operator's data retention settings, laid over what each provider
    /// documents or reads. See [`Self::retention`].
    retention: leviath_providers::retention::RetentionSettings,
}

impl ProviderRegistry {
    /// Create a new empty provider registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a script-provider layer for lazy/hot-reloading `.rhai` providers.
    pub fn with_script_layer(mut self, layer: Arc<ScriptProviderLayer>) -> Self {
        self.script_layer = Some(layer);
        self
    }

    /// The data retention settings from `config.toml`: whether zero
    /// retention is asked for, which providers the organisation holds an
    /// agreement with, and the per-model and per-provider declarations.
    pub fn with_retention(
        mut self,
        settings: leviath_providers::retention::RetentionSettings,
    ) -> Self {
        self.retention = settings;
        self
    }

    /// Replace the settings in place: what the daemon's housekeeper does
    /// when `config.toml` changes under a running daemon.
    pub fn set_retention(&mut self, settings: leviath_providers::retention::RetentionSettings) {
        self.retention = settings;
    }

    /// The settings [`Self::with_retention`] installed.
    pub fn retention_settings(&self) -> &leviath_providers::retention::RetentionSettings {
        &self.retention
    }

    /// [`Self::retention`] with the caller's settings rather than the
    /// registry's own: the spawn gate reads them off the config it was
    /// handed, which is the live one, while the registry's copy follows on
    /// the next housekeeping pass.
    pub fn retention_with(
        &self,
        settings: &leviath_providers::retention::RetentionSettings,
        provider: &str,
        model: &str,
    ) -> leviath_providers::retention::RetentionPolicy {
        let base = self
            .get(provider)
            .and_then(|p| p.live_retention(model))
            .unwrap_or_else(|| leviath_providers::retention::builtin(provider, model));
        leviath_providers::retention::resolve(base, provider, model, settings)
    }

    /// Why `provider`/`model` may not be called, when zero data retention is
    /// asked for in `settings` and the model keeps something; `None` when it
    /// may.
    ///
    /// Worded to follow the model's name ("openai/gpt-5.5, which does not
    /// ..."), so the spawn gate and every inference lane refuse in the same
    /// words, each with its own lead-in.
    pub fn retention_refusal_with(
        &self,
        settings: &leviath_providers::retention::RetentionSettings,
        provider: &str,
        model: &str,
    ) -> Option<String> {
        if !settings.zero_requested {
            return None;
        }
        let policy = self.retention_with(settings, provider, model);
        (!policy.is_zero()).then(|| {
            format!(
                "{provider}/{model}, which does not run with zero data retention \
                 (retention {}: {}). `[providers] zero_retention` is on: name a model \
                 that keeps nothing, declare the agreement in \
                 `zero_retention_agreements` if you hold one, or turn the setting off.",
                policy.retention.describe(),
                policy.note,
            )
        })
    }

    /// [`Self::retention_refusal_with`] with the registry's own settings: what
    /// an inference lane checks just before it sends, so a switch turned on
    /// under a running daemon holds for the calls that follow.
    pub fn retention_refusal(&self, provider: &str, model: &str) -> Option<String> {
        self.retention_refusal_with(&self.retention, provider, model)
    }

    /// What `provider` keeps of `model`'s requests, with the operator's
    /// settings applied: what the provider read from its account if it
    /// could, else the compiled-in table for the name it is registered
    /// under (a script provider or an endpoint answers by its own name),
    /// then a declaration, an agreement, or the request for zero retention
    /// on top. Answers for a provider that is not registered too, from the
    /// table alone, so a listing can describe one that is merely known.
    pub fn retention(
        &self,
        provider: &str,
        model: &str,
    ) -> leviath_providers::retention::RetentionPolicy {
        self.retention_with(&self.retention, provider, model)
    }

    /// Ask every native provider to read again what it answers
    /// [`Provider::live_retention`] from, in turn. A daemon calls this
    /// when zero retention is switched on under it, so an account mode
    /// `lev providers retention` just set is what the next spawn is judged
    /// by rather than what the daemon read when it started.
    pub async fn refresh_retention(&self) {
        // One after another: a handful of providers, of which one or two
        // read anything, each a short side call.
        for provider in self.providers.values() {
            provider.refresh_retention().await;
        }
    }

    /// Merge the per-request zero-retention fields for `provider` into a
    /// request's extra parameters, when zero retention is asked for. A
    /// provider without such fields is left alone.
    pub fn apply_retention_knobs(&self, provider: &str, extra: &mut serde_json::Value) {
        if self.retention.zero_requested {
            // An endpoint that declared which built-in's fields it takes
            // (`zero_retention_request`) is sent that provider's.
            let knobs = self.retention.knob_provider(provider);
            leviath_providers::retention::apply_request_knobs(knobs, extra);
        }
    }

    /// Register a provider by name.
    pub fn register(&mut self, name: String, provider: Arc<dyn Provider>) {
        self.providers.insert(name, provider);
    }

    /// Get a provider by name, returning an owned handle.
    ///
    /// A native provider wins; otherwise the script layer is consulted, which
    /// lazily compiles (or hot-reloads) the matching `.rhai` script.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Provider>> {
        if let Some(p) = self.providers.get(name) {
            return Some(p.clone());
        }
        if let Some(p) = self.retired.get(name) {
            return Some(p.clone());
        }
        self.script_layer.as_ref()?.get_or_load(name)
    }

    /// The script layer this registry loads `.rhai` providers through, if any.
    /// A rebuilt registry takes the old one's layer rather than compiling a
    /// new one, so the scripts it already loaded stay loaded.
    pub fn script_layer(&self) -> Option<Arc<ScriptProviderLayer>> {
        self.script_layer.clone()
    }

    /// Carry forward, as retired, every native provider `previous` had that
    /// this registry does not. Runs that are mid-stage on one of them finish
    /// that stage on it; no new resolution reaches it. A provider this
    /// registry registers again under the same name is live, not retired,
    /// whatever `previous` held.
    pub fn retiring_from(mut self, previous: &ProviderRegistry) -> Self {
        for (name, provider) in previous.providers.iter().chain(previous.retired.iter()) {
            if !self.providers.contains_key(name) {
                self.retired
                    .entry(name.clone())
                    .or_insert_with(|| provider.clone());
            }
        }
        self
    }

    /// The names of the retired providers, sorted; for logs and tests.
    pub fn retired_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.retired.keys().cloned().collect();
        names.sort();
        names
    }

    /// Let every registered provider learn what its own API says about its
    /// models, before the first inference asks.
    ///
    /// Bounded and never fatal. A provider that cannot reach its API keeps its
    /// built-in table, so the worst case is a stale capability list rather
    /// than a daemon that will not start. `timeout` covers each provider
    /// separately: this runs on
    /// the start-up path, and an unreachable endpoint must cost a bounded wait
    /// rather than however long a connect takes to give up.
    ///
    /// Script providers are consulted only when `also` names one - in practice
    /// the machine's `default_provider`, and for `lev validate` the providers
    /// the blueprint in front of it pins. `get` compiles them on demand, so
    /// priming the lot would compile every `.rhai` provider on disk whether or
    /// not a run touches one; priming the ones a caller has actually named
    /// costs a compile of a script that is about to be used anyway, and it is
    /// what lets that provider answer [`Provider::serves_model`] and so win an
    /// open route.
    ///
    /// Answers the providers whose listing failed, each with the error in the
    /// words [`leviath_providers::ProviderError::describe`] gives it, so a
    /// caller can record the failure where `lev setup` will see it. A timeout
    /// is not among them: it says nothing about the credential.
    pub async fn prime_capabilities(
        &self,
        timeout: std::time::Duration,
        also: &[&str],
    ) -> Vec<(String, String)> {
        let mut targets: Vec<(String, Arc<dyn Provider>)> = self
            .providers
            .iter()
            .map(|(name, provider)| (name.clone(), provider.clone()))
            .collect();
        // Every script provider this machine configured, plus the one named as
        // its default.
        //
        // Configured, not every `.rhai` on disk: compiling the lot to ask what
        // each serves is the cost this registry exists to avoid. But a provider
        // with a `[model_providers.<name>]` block is not "a script on disk" -
        // somebody wrote its name and its key down. Leaving those unprimed is
        // what made a working `list_models` go unasked the moment its provider
        // stopped being the default, so the provider claimed no models and no
        // blueprint could route to it without pinning it by name.
        let configured = self
            .script_layer
            .as_ref()
            .map(|l| l.configured_names())
            .unwrap_or_default();
        for name in configured
            .iter()
            .map(String::as_str)
            .chain(also.iter().copied())
        {
            // Only when it is not already registered natively: a native provider
            // of the same name wins everywhere else, and priming it twice would
            // be a second network call for one answer. Nor twice over, for one
            // that is both configured and the default.
            if self.providers.contains_key(name) || targets.iter().any(|(n, _)| n == name) {
                continue;
            }
            if let Some(provider) = self.get(name) {
                targets.push((name.to_string(), provider));
            }
        }
        // Side by side rather than one after another: each provider's answer
        // is its own network call, and a listing command or a daemon start
        // that waited for five of them in turn paid five timeouts in the
        // worst case where one would do.
        let mut failures = Vec::new();
        let mut in_flight = tokio::task::JoinSet::new();
        for (name, provider) in targets {
            in_flight.spawn(async move {
                let outcome = tokio::time::timeout(timeout, provider.prime_capabilities()).await;
                (name, outcome)
            });
        }
        while let Some(joined) = in_flight.join_next().await {
            // A panic inside a provider's priming is that provider's fault
            // and nobody else's; it is reported the way a failed read is.
            let (name, outcome) = match joined {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(error = %e, "a provider's priming task did not finish");
                    continue;
                }
            };
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(
                        provider = %name,
                        error = %e,
                        "could not read this provider's model list, so model sizes \
                         come from the table compiled into this build; a model it \
                         does not name gets a conservative window"
                    );
                    failures.push((name, e.describe()));
                }
                Err(_) => tracing::warn!(
                    provider = %name,
                    timeout_secs = timeout.as_secs(),
                    "timed out reading this provider's model list, so model \
                     sizes come from the table compiled into this build"
                ),
            }
        }
        failures.sort();
        failures
    }

    /// Write every native provider's primed catalogue to the shared capability
    /// cache at `path`, stamped `now` (Unix seconds). Called by the daemon after
    /// a successful prime so short-lived processes read the same numbers. A
    /// provider that keeps no learned store (a script provider) contributes
    /// nothing; a cache that could not be written is a warning, never fatal.
    ///
    /// A primed listing is also a successful check, so each provider that
    /// primed gets a [`ProviderCheck`](leviath_providers::ProviderCheck)
    /// stamped `now`, carrying its entry in `fingerprints` (provider name to
    /// credential fingerprint, made by the caller), which is what lets
    /// `lev setup` open on a provider the daemon has already proved works. The file is added to, not replaced:
    /// checks other surfaces recorded, and providers this registry does not
    /// hold, stay as they were.
    ///
    /// `path` is an `Option` so a caller whose home did not resolve (the cache
    /// path is unknown) passes `None` and this no-ops, keeping the caller
    /// branch-free rather than each guarding an untestable `None`.
    ///
    /// `failures` are the providers whose prime failed, as
    /// [`Self::prime_capabilities`] answered them: each is recorded as a
    /// failed check with that message, so `lev setup` shows the error the
    /// daemon hit instead of a provider that looks unchecked.
    pub fn save_capability_cache(
        &self,
        path: Option<&std::path::Path>,
        now: i64,
        fingerprints: &HashMap<String, String>,
        failures: &[(String, String)],
    ) {
        let Some(path) = path else {
            return;
        };
        let mut cache = leviath_providers::CapabilityCache::load_or_new(path, now);
        for (name, message) in failures {
            cache.record_check(
                name,
                leviath_providers::ProviderCheck {
                    checked_at: now,
                    credential: fingerprints.get(name).cloned(),
                    outcome: leviath_providers::CheckOutcome::Failed {
                        message: message.clone(),
                    },
                },
            );
        }
        for (name, provider) in &self.providers {
            if let Some(learned) = provider.learned_models() {
                let snapshot = learned.snapshot();
                if !snapshot.is_empty() {
                    cache.record_check(
                        name,
                        leviath_providers::ProviderCheck {
                            checked_at: now,
                            credential: fingerprints.get(name).cloned(),
                            outcome: leviath_providers::CheckOutcome::Reachable {
                                models: snapshot.len(),
                            },
                        },
                    );
                    cache.set(name, snapshot);
                }
            }
        }
        if let Err(e) = cache.save(path) {
            tracing::warn!(error = %e, "could not write the model-capability cache");
        }
    }

    /// Fill each native provider's learned store from the cache at `path`, for
    /// the providers it holds an entry for, and report whether anything loaded.
    ///
    /// Lets a freshly built registry answer a model's real limits without its own
    /// network prime, as long as some process (the daemon) has primed and written
    /// the cache. A missing file, or one written by another format version,
    /// loads nothing and returns false: the registry then primes over the
    /// network or answers from the compiled table. The cache's age is not
    /// checked; a listing learned last month is still closer than the table.
    pub fn load_capability_cache(&self, path: &std::path::Path) -> bool {
        let Some(cache) = leviath_providers::CapabilityCache::load(path) else {
            return false;
        };
        let mut loaded = false;
        for (name, provider) in &self.providers {
            let Some(learned) = provider.learned_models() else {
                continue;
            };
            if let Some(models) = cache.get(name) {
                learned.replace(models.clone().into_iter().collect());
                loaded = true;
            }
        }
        loaded
    }

    /// Get every model a run is about to use ready, before it starts.
    ///
    /// `models` is what the blueprint names, bare and deduplicated. Every
    /// provider is asked and each takes the ones it serves: the caller does not
    /// know which model belongs to whom, and a blueprint may name a model with
    /// no provider at all, which is the case this has to keep working.
    ///
    /// Bounded per provider and never fatal. A run whose warm-up timed out is a
    /// run that starts on the compiled table - the same table it would have used
    /// if this had never been called - so the failure costs accuracy, not the
    /// run.
    ///
    /// `also` names a script provider to include, for the same reason
    /// [`prime_capabilities`](Self::prime_capabilities) takes one: a script
    /// provider is compiled on demand, so the ones on disk are not enumerable
    /// here, and the machine's default is the one worth the compile.
    pub async fn warm_models(
        &self,
        models: &[String],
        timeout: std::time::Duration,
        also: Option<&str>,
    ) {
        if models.is_empty() {
            return;
        }
        let mut targets: Vec<(String, Arc<dyn Provider>)> = self
            .providers
            .iter()
            .map(|(name, provider)| (name.clone(), provider.clone()))
            .collect();
        if let Some(name) = also
            && !self.providers.contains_key(name)
            && let Some(provider) = self.get(name)
        {
            targets.push((name.to_string(), provider));
        }
        for (name, provider) in targets {
            match tokio::time::timeout(timeout, provider.warm_models(models)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(
                    provider = %name,
                    error = %e,
                    "could not warm this provider's models before the run, so any \
                     it serves are sized from the table compiled into this build"
                ),
                Err(_) => tracing::warn!(
                    provider = %name,
                    timeout_secs = timeout.as_secs(),
                    "timed out warming this provider's models before the run, so \
                     any it serves are sized from the table compiled into this build"
                ),
            }
        }
    }

    /// Check if a provider is available: registered natively, or resolvable
    /// (loadable) as a script provider right now. Used at stage-model selection;
    /// network-free because script `initialize` runs offline.
    pub fn has(&self, name: &str) -> bool {
        self.providers.contains_key(name)
            || self
                .script_layer
                .as_ref()
                .is_some_and(|l| l.get_or_load(name).is_some())
    }

    /// Whether `provider` is registered here and can prove it does not serve
    /// `model`.
    ///
    /// `false` is the answer to three different questions - there is no such
    /// provider, it serves the model, or it will not say what it serves - and
    /// they are folded together on purpose. Every caller of this is deciding
    /// whether to refuse something, and all three mean "do not refuse". Only a
    /// provider that published a complete catalogue can put a name outside it,
    /// so a `true` here is always evidence rather than an absence of it.
    ///
    /// Compared by [`crate::pipeline::model_key`], so a gateway's namespaced
    /// `openai/gpt-5.5` answers for a blueprint that wrote `gpt-5.5`.
    pub(crate) fn refuses_model(&self, provider: &str, model: &str) -> bool {
        let Some(p) = self.get(provider) else {
            return false;
        };
        let Some(catalog) = p.served_catalog() else {
            return false;
        };
        let key = crate::pipeline::model_key(model);
        !catalog
            .iter()
            .any(|id| crate::pipeline::model_key(id) == key)
    }

    /// What `provider` says about refusing `model`, when it has more to say
    /// than the absence itself.
    ///
    /// Asked only after [`refuses_model`](Self::refuses_model) has already
    /// said no, so this is about the wording of a decision rather than the
    /// decision.
    pub(crate) fn refusal_reason(&self, provider: &str, model: &str) -> Option<String> {
        self.get(provider)?
            .refusal_reason(crate::pipeline::model_key(model))
    }

    /// Get all *natively-registered* provider names. Script providers are
    /// resolved on demand and so are not enumerated here - see
    /// [`resolvable_names`](Self::resolvable_names) for the set that includes
    /// them.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.keys().map(|k| k.as_str()).collect()
    }

    /// The script provider registered under `name`, when there is one and it is
    /// not shadowed by a native provider of the same name.
    ///
    /// The narrow counterpart to [`Self::native_providers`]: it resolves one
    /// name rather than enumerating, so it compiles exactly the script asked
    /// for. That is what makes it safe on the resolve path, where enumerating
    /// would compile every `.rhai` on disk.
    pub fn script_provider_named(&self, name: &str) -> Option<Arc<dyn Provider>> {
        if self.providers.contains_key(name) {
            return None;
        }
        self.script_layer.as_ref()?.get_or_load(name)
    }

    /// Every natively registered provider, with the name it is registered under.
    ///
    /// The pair form for callers that ask each provider a question rather than
    /// looking one up by name: [`Self::provider_names`] plus [`Self::get`] leaves
    /// the caller holding an `Option` that cannot be `None`, because both read
    /// the same map. Script providers are excluded for the reason
    /// [`Self::prime_capabilities`] gives: `get` compiles them on demand.
    pub fn native_providers(&self) -> Vec<(&str, Arc<dyn Provider>)> {
        self.providers
            .iter()
            .map(|(name, provider)| (name.as_str(), provider.clone()))
            .collect()
    }

    /// Every provider name this registry could answer for right now: the
    /// natively registered ones, then the script providers the layer can see.
    ///
    /// This is what an *enumeration* wants - "list every model I can reach" -
    /// where [`provider_names`](Self::provider_names) answers "what is
    /// registered". An enumeration built on `provider_names` alone silently
    /// omits every script provider, which is what `lev models list` and
    /// `GET /api/models` both need from here.
    ///
    /// A script name here is a candidate: [`get`](Self::get) compiles it on
    /// demand and returns `None` if it will not load, so a caller iterating
    /// this must handle a name it cannot resolve.
    pub fn resolvable_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.providers.keys().cloned().collect();
        if let Some(layer) = &self.script_layer {
            for name in layer.candidate_names() {
                // A native provider of the same name wins, exactly as `get`
                // resolves it, so it is never listed twice.
                if !names.iter().any(|n| n == &name) {
                    names.push(name);
                }
            }
        }
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviath_providers::{
        InferenceRequest, InferenceResponse, ModelCapabilities, ProviderError,
    };

    /// What a stub does when asked to prime.
    enum PrimeOutcome {
        Ok,
        Fails,
        Hangs,
        Panics,
    }

    struct StubProvider {
        primed: Arc<std::sync::atomic::AtomicUsize>,
        outcome: PrimeOutcome,
        /// Every list this stub was handed to warm, in order.
        warmed: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
        /// The catalogue this stub publishes, if it publishes one. `None` is
        /// the ordinary fixture and means "will not say", which no caller may
        /// read as a refusal.
        catalog: Option<Vec<String>>,
        /// What it says about refusing anything outside that catalogue.
        refusal: Option<String>,
        /// The learned store the capability cache reads and fills.
        learned: leviath_providers::LearnedModels,
        /// When true, [`Provider::learned_models`] returns `None`, standing in
        /// for a provider (a script provider) that keeps no learned store.
        no_store: bool,
    }

    impl StubProvider {
        fn new(outcome: PrimeOutcome) -> Self {
            Self {
                primed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                outcome,
                warmed: Arc::new(std::sync::Mutex::new(Vec::new())),
                catalog: None,
                refusal: None,
                learned: leviath_providers::LearnedModels::default(),
                no_store: false,
            }
        }

        /// A stub that keeps no learned store, like a script provider.
        fn storeless() -> Self {
            Self {
                no_store: true,
                ..Self::new(PrimeOutcome::Ok)
            }
        }

        /// A stub whose learned store already holds `models` (id → context
        /// window), as if it had primed.
        fn with_learned(models: &[(&str, usize)]) -> Self {
            let stub = Self::new(PrimeOutcome::Ok);
            stub.learned.replace(
                models
                    .iter()
                    .map(|(id, ctx)| {
                        (
                            (*id).to_string(),
                            leviath_providers::LearnedModel {
                                max_context_tokens: Some(*ctx),
                                ..Default::default()
                            },
                        )
                    })
                    .collect(),
            );
            stub
        }

        /// The same stub, publishing a complete catalogue.
        fn publishing(models: &[&str]) -> Self {
            Self {
                catalog: Some(models.iter().map(|m| (*m).to_string()).collect()),
                ..Self::new(PrimeOutcome::Ok)
            }
        }

        /// And one that explains what it will not serve.
        fn explaining(models: &[&str], reason: &str) -> Self {
            Self {
                refusal: Some(reason.to_string()),
                ..Self::publishing(models)
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for StubProvider {
        async fn warm_models(&self, models: &[String]) -> Result<(), ProviderError> {
            self.warmed
                .lock()
                .expect("not poisoned")
                .push(models.to_vec());
            match self.outcome {
                PrimeOutcome::Ok => Ok(()),
                PrimeOutcome::Fails => Err(ProviderError::ApiError("no".to_string())),
                PrimeOutcome::Hangs => {
                    // Longer than any timeout a test passes.
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    Ok(())
                }
                PrimeOutcome::Panics => panic!("a provider bug"),
            }
        }

        async fn prime_capabilities(&self) -> Result<(), ProviderError> {
            self.primed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            match self.outcome {
                PrimeOutcome::Ok => Ok(()),
                PrimeOutcome::Fails => Err(ProviderError::ApiError("no".to_string())),
                PrimeOutcome::Hangs => {
                    // Longer than any timeout a test passes, so the timeout arm
                    // is what ends this rather than the sleep.
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    Ok(())
                }
                PrimeOutcome::Panics => panic!("a provider bug"),
            }
        }
        async fn infer(
            &self,
            _request: &InferenceRequest,
        ) -> Result<InferenceResponse, ProviderError> {
            Err(ProviderError::ApiError("stub".to_string()))
        }
        async fn count_tokens(&self, text: &str, _model: &str) -> usize {
            text.len()
        }
        fn max_context_tokens(&self, _model: &str) -> usize {
            8192
        }
        fn name(&self) -> &str {
            "stub"
        }
        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }
        fn served_catalog(&self) -> Option<Vec<String>> {
            self.catalog.clone()
        }

        fn learned_models(&self) -> Option<&leviath_providers::LearnedModels> {
            (!self.no_store).then_some(&self.learned)
        }

        fn refusal_reason(&self, _model_key: &str) -> Option<String> {
            self.refusal.clone()
        }
    }

    /// A provider gets to explain a refusal, and one with nothing to add is
    /// silent rather than inventing a sentence.
    /// A retention answer is keyed by the name a provider is registered
    /// under, not the name it calls itself: a stub registered as `openai`
    /// answers with OpenAI's documented policy, and the operator's settings
    /// sit on top. The per-request knobs go out only when zero retention was
    /// asked for.
    #[test]
    fn retention_is_answered_by_registry_name_with_settings_on_top() {
        use leviath_providers::retention::{Retention, RetentionSettings, Source};
        let mut registry = ProviderRegistry::new();
        registry.register(
            "openai".to_string(),
            Arc::new(StubProvider::new(PrimeOutcome::Ok)),
        );
        let documented = registry.retention("openai", "gpt-5.5");
        assert_eq!(documented.retention, Retention::Days(30));
        assert_eq!(documented.source, Source::Builtin);
        // Unregistered is still described, from the table.
        assert!(registry.retention("ollama", "qwen3.5:9b").is_zero());
        let mut extra = serde_json::Value::Null;
        registry.apply_retention_knobs("openai", &mut extra);
        assert!(extra.is_null(), "nothing asked for, nothing sent");

        let settings = RetentionSettings {
            zero_requested: true,
            agreements: vec!["openai".to_string()],
            ..Default::default()
        };
        let mut registry = registry.with_retention(settings.clone());
        assert_eq!(registry.retention_settings(), &settings);
        // Replaced in place, the way the housekeeper does it, and answered
        // with a caller's settings, the way the spawn gate does it.
        registry.set_retention(RetentionSettings::default());
        assert!(!registry.retention("openai", "gpt-5.5").is_zero());
        assert!(
            registry
                .retention_with(&settings, "openai", "gpt-5.5")
                .is_zero()
        );
        registry.set_retention(settings.clone());
        let declared = registry.retention("openai", "gpt-5.5");
        assert!(declared.is_zero());
        assert_eq!(declared.source, Source::Declared);
        registry.apply_retention_knobs("openai", &mut extra);
        assert_eq!(extra, serde_json::json!({ "store": false }));

        // An endpoint that declared whose fields it takes is sent them; one
        // that did not is sent nothing, since the table has none for it.
        registry.set_retention(RetentionSettings {
            zero_requested: true,
            request_knob_aliases: HashMap::from([("azure".to_string(), "openai".to_string())]),
            ..Default::default()
        });
        let mut azure = serde_json::Value::Null;
        registry.apply_retention_knobs("azure", &mut azure);
        assert_eq!(azure, serde_json::json!({ "store": false }));
        let mut plain = serde_json::Value::Null;
        registry.apply_retention_knobs("gw", &mut plain);
        assert!(plain.is_null());
    }

    /// The refusal every lane shares: nothing when zero retention is off or
    /// the model keeps nothing, and otherwise the model named with why.
    #[test]
    fn a_retention_refusal_names_the_model_and_the_way_out() {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "openai".to_string(),
            Arc::new(StubProvider::new(PrimeOutcome::Ok)),
        );
        assert_eq!(registry.retention_refusal("openai", "gpt-5.5"), None);
        let registry = registry.with_retention(leviath_providers::retention::RetentionSettings {
            zero_requested: true,
            ..Default::default()
        });
        let refusal = registry
            .retention_refusal("openai", "gpt-5.5")
            .expect("OpenAI keeps an abuse log");
        assert!(
            refusal.starts_with("openai/gpt-5.5, which does not run with zero data retention"),
            "{refusal}"
        );
        assert!(refusal.contains("zero_retention_agreements"), "{refusal}");
        assert_eq!(registry.retention_refusal("ollama", "qwen3.5:9b"), None);
    }

    #[test]
    fn refusal_reason_comes_from_the_provider_or_not_at_all() {
        let mut reg = ProviderRegistry::new();
        reg.register(
            "codexish".to_string(),
            Arc::new(StubProvider::explaining(
                &["gpt-5.5"],
                "your plan does not include it",
            )),
        );
        reg.register(
            "groq".to_string(),
            Arc::new(StubProvider::publishing(&["llama-4-scout"])),
        );

        assert_eq!(
            reg.refusal_reason("codexish", "gpt-5.3-spark").as_deref(),
            Some("your plan does not include it")
        );
        assert_eq!(reg.refusal_reason("groq", "llama-3.1-70b"), None);
        // And a provider that is not here explains nothing rather than
        // panicking on the lookup.
        assert_eq!(reg.refusal_reason("absent", "anything"), None);
    }

    /// `refuses_model` folds three different "no" answers into `false`, because
    /// every caller of it is deciding whether to refuse and all three mean "do
    /// not". Only a published catalogue can produce a `true`.
    #[test]
    fn refuses_model_only_says_yes_on_a_published_catalogue() {
        let mut reg = ProviderRegistry::new();
        reg.register(
            "groq".to_string(),
            Arc::new(StubProvider::publishing(&["llama-4-scout"])),
        );
        // A native provider that publishes nothing: silence, never a refusal.
        reg.register("quiet".to_string(), mock());

        assert!(
            reg.refuses_model("groq", "llama-3.1-70b"),
            "not in the list"
        );
        assert!(!reg.refuses_model("groq", "llama-4-scout"), "in the list");
        assert!(!reg.refuses_model("quiet", "anything"), "published nothing");
        assert!(!reg.refuses_model("absent", "anything"), "no such provider");
    }

    /// A gateway namespaces its ids and a blueprint names the model, so the two
    /// are compared by model key. Comparing raw strings would make every
    /// gateway route look like a model the gateway refuses.
    #[test]
    fn refuses_model_compares_by_model_key() {
        let mut reg = ProviderRegistry::new();
        reg.register(
            "openrouter".to_string(),
            Arc::new(StubProvider::publishing(&["openai/gpt-5.5"])),
        );

        assert!(!reg.refuses_model("openrouter", "gpt-5.5"));
        assert!(!reg.refuses_model("openrouter", "openai/gpt-5.5"));
        assert!(reg.refuses_model("openrouter", "gpt-4"));
    }

    /// What an *enumeration* needs, and what `provider_names` cannot give it:
    /// the script providers too. `GET /api/models` and
    /// `lev models list --remote` both read this list, so a script provider
    /// has to appear in it.
    #[test]
    fn resolvable_names_adds_the_script_layer_without_duplicating_a_native() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("scripted.rhai"),
            "fn initialize(c) { #{} }\nfn inference(s, r) { #{ content: \"x\" } }",
        )
        .unwrap();
        // A script of the same name as a native provider: `get` prefers the
        // native one, so it must be listed once.
        std::fs::write(
            dir.path().join("native.rhai"),
            "fn initialize(c) { #{} }\nfn inference(s, r) { #{ content: \"x\" } }",
        )
        .unwrap();

        let mut registry = ProviderRegistry::new();
        registry.register("native".to_string(), mock());
        assert_eq!(
            registry.resolvable_names(),
            vec!["native".to_string()],
            "with no layer attached this is just the registered set"
        );

        let layer = crate::script_provider::ScriptProviderLayer::new(
            dir.path().to_path_buf(),
            HashMap::new(),
            HashMap::new(),
            None,
            Vec::new(),
        );
        let registry = registry.with_script_layer(Arc::new(layer));

        let mut names = registry.resolvable_names();
        names.sort();
        assert_eq!(names, vec!["native".to_string(), "scripted".to_string()]);
        assert_eq!(
            registry.provider_names(),
            vec!["native"],
            "provider_names keeps its own contract"
        );
    }

    fn mock() -> Arc<dyn Provider> {
        Arc::new(StubProvider::new(PrimeOutcome::Ok))
    }

    #[test]
    fn register_get_has_and_names() {
        let mut reg = ProviderRegistry::new();
        assert!(!reg.has("anthropic"));
        assert!(reg.get("anthropic").is_none());
        reg.register("anthropic".to_string(), mock());
        assert!(reg.has("anthropic"));
        assert!(reg.get("anthropic").is_some());
        assert_eq!(reg.provider_names(), vec!["anthropic"]);
    }

    #[test]
    fn default_is_empty() {
        let reg = ProviderRegistry::default();
        assert!(reg.provider_names().is_empty());
    }

    fn priming(outcome: PrimeOutcome) -> (Arc<dyn Provider>, Arc<std::sync::atomic::AtomicUsize>) {
        let primed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (
            Arc::new(StubProvider {
                primed: primed.clone(),
                ..StubProvider::new(outcome)
            }),
            primed,
        )
    }

    /// Every provider is asked, because the caller does not know which model
    /// belongs to whom - and a blueprint may name one with no provider at all,
    /// which is the case this has to keep working.
    #[tokio::test]
    async fn warming_asks_every_provider_for_the_whole_list() {
        let a = Arc::new(StubProvider::new(PrimeOutcome::Ok));
        let b = Arc::new(StubProvider::new(PrimeOutcome::Ok));
        let mut registry = ProviderRegistry::new();
        registry.register("a".to_string(), a.clone());
        registry.register("b".to_string(), b.clone());

        let models = vec!["one".to_string(), "two".to_string()];
        registry
            .warm_models(&models, std::time::Duration::from_secs(5), None)
            .await;

        for stub in [&a, &b] {
            let seen = stub.warmed.lock().expect("not poisoned").clone();
            assert_eq!(seen, vec![models.clone()], "asked once, with everything");
        }
    }

    /// Nothing to warm means nobody is disturbed - a run whose blueprint names
    /// no models should not cost a round of provider calls.
    #[tokio::test]
    async fn warming_nothing_asks_nobody() {
        let stub = Arc::new(StubProvider::new(PrimeOutcome::Ok));
        let mut registry = ProviderRegistry::new();
        registry.register("a".to_string(), stub.clone());

        registry
            .warm_models(&[], std::time::Duration::from_secs(5), None)
            .await;

        assert!(stub.warmed.lock().expect("not poisoned").is_empty());
    }

    /// A provider that fails or hangs does not stop the run: warming is an
    /// improvement on the compiled table, and a run that could not be warmed
    /// still runs on it.
    #[tokio::test]
    async fn a_failing_or_hanging_provider_does_not_block_the_run() {
        let fails = Arc::new(StubProvider::new(PrimeOutcome::Fails));
        let hangs = Arc::new(StubProvider::new(PrimeOutcome::Hangs));
        let mut registry = ProviderRegistry::new();
        registry.register("fails".to_string(), fails.clone());
        registry.register("hangs".to_string(), hangs.clone());

        // Returns rather than hanging, which is the whole assertion.
        registry
            .warm_models(
                &["m".to_string()],
                std::time::Duration::from_millis(50),
                None,
            )
            .await;

        assert_eq!(fails.warmed.lock().expect("not poisoned").len(), 1);
        assert_eq!(hangs.warmed.lock().expect("not poisoned").len(), 1);
    }

    /// A failed listing comes back named, in the words setup shows; one that
    /// only ran out of time does not, since it says nothing about the key.
    #[tokio::test]
    async fn priming_answers_the_providers_whose_listing_failed() {
        let mut reg = ProviderRegistry::new();
        reg.register(
            "fails".to_string(),
            Arc::new(StubProvider::new(PrimeOutcome::Fails)),
        );
        reg.register(
            "hangs".to_string(),
            Arc::new(StubProvider::new(PrimeOutcome::Hangs)),
        );
        reg.register(
            "ok".to_string(),
            Arc::new(StubProvider::new(PrimeOutcome::Ok)),
        );
        let failures = reg
            .prime_capabilities(std::time::Duration::from_millis(100), &[])
            .await;
        assert_eq!(
            failures,
            vec![("fails".to_string(), "API error: no".to_string())]
        );
    }

    #[tokio::test]
    async fn priming_reaches_every_registered_provider() {
        let mut reg = ProviderRegistry::new();
        let (p, primed) = priming(PrimeOutcome::Ok);
        reg.register("prime".to_string(), p);
        reg.register("other".to_string(), mock());

        reg.prime_capabilities(std::time::Duration::from_secs(5), &[])
            .await;
        assert_eq!(primed.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// Priming runs side by side now, so one provider's bug is caught at its
    /// task boundary and reported; the others still prime and the daemon
    /// still starts.
    #[tokio::test]
    async fn a_panicking_prime_is_reported_and_the_rest_still_prime() {
        let mut reg = ProviderRegistry::new();
        let (bug, _) = priming(PrimeOutcome::Panics);
        let (good, good_calls) = priming(PrimeOutcome::Ok);
        reg.register("bug".to_string(), bug);
        reg.register("good".to_string(), good);
        reg.prime_capabilities(std::time::Duration::from_secs(5), &[])
            .await;
        assert_eq!(good_calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// Side by side, not one after another: two providers that each take the
    /// whole timeout finish in one timeout, not two.
    #[tokio::test]
    async fn providers_prime_concurrently() {
        let mut reg = ProviderRegistry::new();
        let (a, _) = priming(PrimeOutcome::Hangs);
        let (b, _) = priming(PrimeOutcome::Hangs);
        reg.register("a".to_string(), a);
        reg.register("b".to_string(), b);
        let started = std::time::Instant::now();
        reg.prime_capabilities(std::time::Duration::from_millis(200), &[])
            .await;
        assert!(started.elapsed() < std::time::Duration::from_millis(390));
    }

    /// A provider that cannot answer is a warning, not a failure: the daemon
    /// has to start whether or not an API is reachable.
    #[tokio::test]
    async fn a_failing_prime_does_not_stop_the_rest() {
        let mut reg = ProviderRegistry::new();
        let (bad, bad_calls) = priming(PrimeOutcome::Fails);
        reg.register("bad".to_string(), bad);
        reg.prime_capabilities(std::time::Duration::from_secs(5), &[])
            .await;
        assert_eq!(bad_calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// An endpoint that never answers costs the timeout, not the start-up.
    #[tokio::test(start_paused = true)]
    async fn priming_gives_up_on_a_provider_that_hangs() {
        let mut reg = ProviderRegistry::new();
        let (slow, calls) = priming(PrimeOutcome::Hangs);
        reg.register("slow".to_string(), slow);
        // With the clock paused this returns as soon as the timeout is the only
        // thing left to wait on, so a regression here fails by hanging the
        // suite rather than by sleeping through it.
        reg.prime_capabilities(std::time::Duration::from_secs(10), &[])
            .await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn script_layer_resolves_and_native_wins() {
        use crate::script_provider::ScriptProviderLayer;
        use std::collections::HashMap;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("groq.rhai"),
            "fn initialize(config) { #{} }\nfn inference(state, request) { #{ content: \"ok\" } }",
        )
        .unwrap();
        let layer = ScriptProviderLayer::new(
            dir.path().to_path_buf(),
            HashMap::new(),
            HashMap::new(),
            None,
            Vec::new(),
        );
        let mut reg = ProviderRegistry::new().with_script_layer(Arc::new(layer));
        reg.register("anthropic".to_string(), mock());

        // Native provider still wins and is found by name.
        assert!(reg.has("anthropic"));
        assert!(reg.get("anthropic").is_some());

        // A script provider is resolved lazily through the layer by both has/get.
        assert!(reg.has("groq"));
        let p = reg.get("groq").expect("script provider resolves");
        assert_eq!(p.name(), "groq");

        // An unknown name resolves to nothing (layer returns None).
        assert!(!reg.has("nope"));
        assert!(reg.get("nope").is_none());
    }

    /// The narrow lookup that lets the resolve path reach one script provider
    /// without enumerating them all.
    #[test]
    fn one_script_provider_resolves_by_name_and_a_native_shadows_it() {
        use crate::script_provider::ScriptProviderLayer;
        use std::collections::HashMap;

        let dir = tempfile::tempdir().unwrap();
        for name in ["spark", "other"] {
            std::fs::write(
                dir.path().join(format!("{name}.rhai")),
                "fn initialize(config) { #{} }\n\
                 fn inference(state, request) { #{ content: \"ok\" } }",
            )
            .unwrap();
        }
        let layer = ScriptProviderLayer::new(
            dir.path().to_path_buf(),
            HashMap::new(),
            HashMap::new(),
            None,
            Vec::new(),
        );
        let mut reg = ProviderRegistry::new().with_script_layer(Arc::new(layer));

        // The one asked for, and nothing else.
        assert!(reg.script_provider_named("spark").is_some());
        assert!(reg.script_provider_named("nope").is_none());

        // A native provider of the same name shadows the script, so this
        // answers None rather than handing back a provider `get` would not.
        reg.register("spark".to_string(), mock());
        assert!(reg.script_provider_named("spark").is_none());
    }

    /// A registry with no script layer at all has no script to name.
    #[test]
    fn a_registry_without_a_script_layer_names_no_script_provider() {
        assert!(
            ProviderRegistry::new()
                .script_provider_named("spark")
                .is_none()
        );
    }

    /// Priming reaches the script provider a machine names as its default, so
    /// it can answer what it serves on the synchronous resolve path.
    /// A script provider is compiled on demand rather than enumerated, so the
    /// ones on disk are invisible to the loop above. The machine's default is
    /// reached the same way priming reaches it - otherwise a run whose models
    /// live on a local script provider warms everything except the one provider
    /// that needed it.
    #[tokio::test]
    async fn warming_also_reaches_the_named_script_provider() {
        use crate::script_provider::ScriptProviderLayer;
        use std::collections::HashMap;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("spark.rhai"),
            "fn initialize(config) { #{} }\n\
             fn inference(state, request) { #{ content: \"ok\" } }\n\
             fn warm_models(state, models) { if models[0] != \"local-fast\" \
             { throw \"got \" + models[0] } }\n\
             fn list_models(state) { [ #{ id: \"local-fast\", display_name: \"F\", \
             max_context_tokens: 4096, max_output_tokens: 512 } ] }",
        )
        .unwrap();
        let layer = ScriptProviderLayer::new(
            dir.path().to_path_buf(),
            HashMap::new(),
            HashMap::new(),
            None,
            Vec::new(),
        );
        let reg = ProviderRegistry::new().with_script_layer(Arc::new(layer));

        // The script throws unless it is handed exactly this, and a throw is
        // swallowed as a warning - so the assertion is that the call reached it
        // at all, which `serves_model` answering afterwards demonstrates.
        reg.warm_models(
            &["local-fast".to_string()],
            std::time::Duration::from_secs(5),
            Some("spark"),
        )
        .await;

        assert_eq!(
            reg.get("spark")
                .expect("resolves")
                .serves_model("local-fast"),
            None,
            "warming does not prime; the two are separate questions"
        );
    }

    /// The bug this fixes, in the shape it was found in: a script provider with
    /// a config block, a working `list_models`, and a machine whose
    /// `default_provider` is something else. It claimed no models, so no
    /// blueprint could route to it without pinning it by name, and its
    /// `list_models` was never asked.
    #[tokio::test]
    async fn priming_reaches_a_configured_script_provider_that_is_not_the_default() {
        use crate::script_provider::{ScriptProviderLayer, ScriptProviderSpec};
        use std::collections::HashMap;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("cerebras.rhai"),
            "fn initialize(config) { #{} }\n\
             fn inference(state, request) { #{ content: \"ok\" } }\n\
             fn list_models(state) { [ #{ id: \"gpt-oss-120b\", display_name: \"O\", \
             max_context_tokens: 131000, max_output_tokens: 8192 } ] }",
        )
        .unwrap();

        let mut overrides = HashMap::new();
        overrides.insert("cerebras".to_string(), ScriptProviderSpec::default());
        let layer = ScriptProviderLayer::new(
            dir.path().to_path_buf(),
            overrides,
            HashMap::new(),
            None,
            Vec::new(),
        );
        let reg = ProviderRegistry::new().with_script_layer(Arc::new(layer));

        // The default is something else entirely, which is the whole point.
        reg.prime_capabilities(std::time::Duration::from_secs(5), &["openrouter"])
            .await;

        assert_eq!(
            reg.get("cerebras")
                .expect("resolves")
                .serves_model("gpt-oss-120b"),
            Some("gpt-oss-120b".to_string()),
            "a configured provider is asked what it serves even when it is not \
             the default"
        );
    }

    #[tokio::test]
    async fn priming_also_reaches_the_named_script_provider() {
        use crate::script_provider::ScriptProviderLayer;
        use std::collections::HashMap;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("spark.rhai"),
            "fn initialize(config) { #{} }\n\
             fn inference(state, request) { #{ content: \"ok\" } }\n\
             fn list_models(state) { [ #{ id: \"local-fast\", display_name: \"F\", \
             max_context_tokens: 4096, max_output_tokens: 512 } ] }",
        )
        .unwrap();
        let layer = ScriptProviderLayer::new(
            dir.path().to_path_buf(),
            HashMap::new(),
            HashMap::new(),
            None,
            Vec::new(),
        );
        let reg = ProviderRegistry::new().with_script_layer(Arc::new(layer));

        // Unprimed it claims nothing, which is the state that made a local
        // model unreachable.
        assert_eq!(
            reg.get("spark")
                .expect("resolves")
                .serves_model("local-fast"),
            None
        );

        reg.prime_capabilities(std::time::Duration::from_secs(5), &["spark"])
            .await;

        assert_eq!(
            reg.get("spark")
                .expect("resolves")
                .serves_model("local-fast"),
            Some("local-fast".to_string())
        );
    }

    /// Naming a provider that is already registered natively does not prime it
    /// twice: one answer is worth one network call.
    #[tokio::test]
    async fn naming_a_native_provider_does_not_prime_it_twice() {
        let mut reg = ProviderRegistry::new();
        let (p, primed) = priming(PrimeOutcome::Ok);
        reg.register("prime".to_string(), p);

        reg.prime_capabilities(std::time::Duration::from_secs(5), &["prime"])
            .await;
        assert_eq!(primed.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// Naming something that resolves to nothing is not an error - a machine
    /// may name a default it has not set up yet.
    #[tokio::test]
    async fn naming_a_provider_that_does_not_resolve_is_harmless() {
        let reg = ProviderRegistry::new();
        reg.prime_capabilities(std::time::Duration::from_secs(5), &["nope"])
            .await;
    }

    /// A refresh asks every registered provider in turn; a provider that
    /// reads no setting has nothing to do, and the registry's answer for it
    /// is the table's before and after.
    #[tokio::test]
    async fn a_refresh_asks_every_registered_provider() {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "openai".to_string(),
            Arc::new(StubProvider::new(PrimeOutcome::Ok)),
        );
        registry.register(
            "ollama".to_string(),
            Arc::new(StubProvider::new(PrimeOutcome::Ok)),
        );
        let before = registry.retention("openai", "gpt-5.5");
        registry.refresh_retention().await;
        assert_eq!(registry.retention("openai", "gpt-5.5"), before);
        assert!(registry.retention("ollama", "q").is_zero());
    }

    #[tokio::test]
    async fn stub_provider_methods_are_exercised() {
        let p = StubProvider::new(PrimeOutcome::Ok);
        assert_eq!(p.name(), "stub");
        assert_eq!(p.count_tokens("abcd", "m").await, 4);
        assert_eq!(p.max_context_tokens("m"), 8192);
        let _ = p.capabilities("m");
        let request = InferenceRequest {
            system: Vec::new(),
            messages: Vec::new(),
            model: "m".to_string(),
            max_tokens: 10,
            temperature: 0.0,
            tools: Vec::new(),
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        assert!(p.infer(&request).await.is_err());
    }

    #[test]
    fn the_capability_cache_round_trips_and_skips_every_other_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("caps").join("model_capabilities.json");

        // A primed registry with all three provider shapes:
        let mut primed = ProviderRegistry::new();
        // one that learned a real window (cached),
        primed.register(
            "learned".to_string(),
            Arc::new(StubProvider::with_learned(&[("big-model", 200_000)])),
        );
        // one with a store that primed nothing (empty snapshot, not cached),
        primed.register(
            "empty".to_string(),
            Arc::new(StubProvider::new(PrimeOutcome::Ok)),
        );
        // and one with no store at all (a script provider; skipped).
        primed.register("storeless".to_string(), Arc::new(StubProvider::storeless()));
        // A check another surface recorded earlier survives the daemon's save.
        let mut earlier = leviath_providers::CapabilityCache::new(500);
        earlier.record_check(
            "elsewhere",
            leviath_providers::ProviderCheck {
                checked_at: 500,
                credential: None,
                outcome: leviath_providers::CheckOutcome::Failed {
                    message: "rejected".to_string(),
                },
            },
        );
        earlier.save(&path).expect("the earlier cache writes");
        let fingerprints = HashMap::from([("learned".to_string(), "abcd1234abcd1234".to_string())]);
        primed.save_capability_cache(
            Some(&path),
            1_000,
            &fingerprints,
            &[(
                "broken".to_string(),
                "[dns-failure] no such host".to_string(),
            )],
        );

        // The prime is recorded as a check, with the credential it used.
        let written = leviath_providers::CapabilityCache::load(&path).expect("written");
        let check = written
            .check("learned")
            .expect("a primed provider is checked");
        assert_eq!(check.checked_at, 1_000);
        assert_eq!(check.credential.as_deref(), Some("abcd1234abcd1234"));
        assert_eq!(
            check.outcome,
            leviath_providers::CheckOutcome::Reachable { models: 1 }
        );
        assert!(
            written.check("empty").is_none(),
            "a provider that primed nothing is not a passed check"
        );
        assert_eq!(
            written
                .check("broken")
                .expect("a failed prime is recorded")
                .outcome,
            leviath_providers::CheckOutcome::Failed {
                message: "[dns-failure] no such host".to_string()
            }
        );
        assert_eq!(
            written.check("elsewhere").expect("kept").checked_at,
            500,
            "another surface's check survives"
        );

        // A fresh registry reads it back.
        let mut fresh = ProviderRegistry::new();
        // this one is in the cache, so it is filled,
        fresh.register(
            "learned".to_string(),
            Arc::new(StubProvider::new(PrimeOutcome::Ok)),
        );
        // this one has a store but no cache entry, so it is left alone,
        fresh.register(
            "unknown".to_string(),
            Arc::new(StubProvider::new(PrimeOutcome::Ok)),
        );
        // and this one has no store, so it is skipped.
        fresh.register("storeless".to_string(), Arc::new(StubProvider::storeless()));
        assert!(
            fresh
                .get("learned")
                .unwrap()
                .learned_models()
                .unwrap()
                .is_empty(),
            "nothing learned before the cache is loaded"
        );
        assert!(fresh.load_capability_cache(&path));
        assert_eq!(
            fresh
                .get("learned")
                .unwrap()
                .learned_models()
                .unwrap()
                .get("big-model")
                .unwrap()
                .max_context_tokens,
            Some(200_000),
        );
        assert!(
            fresh
                .get("unknown")
                .unwrap()
                .learned_models()
                .unwrap()
                .is_empty(),
            "a provider with no cache entry is left as it was"
        );

        // A missing (or stale-versioned) cache loads nothing and says so.
        assert!(!fresh.load_capability_cache(&dir.path().join("nope.json")));
    }

    #[test]
    fn saving_the_cache_where_it_cannot_be_written_warns_but_does_not_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A file where a directory would have to be makes the write fail.
        let file = dir.path().join("in-the-way");
        std::fs::write(&file, "x").expect("write the blocker");
        let unwritable = file.join("sub").join("model_capabilities.json");
        let mut reg = ProviderRegistry::new();
        reg.register(
            "stub".to_string(),
            Arc::new(StubProvider::with_learned(&[("m", 1)])),
        );
        // The failure is logged, not propagated: a cache is a convenience.
        reg.save_capability_cache(Some(&unwritable), 1, &HashMap::new(), &[]);
        // No path (home did not resolve) is a silent no-op, not a panic.
        reg.save_capability_cache(None, 1, &HashMap::new(), &[]);
    }
}
