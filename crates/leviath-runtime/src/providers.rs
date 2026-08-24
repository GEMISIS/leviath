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
    /// Lazy, hot-reloading resolver for `.rhai` script providers. Shared across
    /// registry clones (one compile cache daemon-wide).
    script_layer: Option<Arc<ScriptProviderLayer>>,
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
        self.script_layer.as_ref()?.get_or_load(name)
    }

    /// Let every registered provider learn what its own API says about its
    /// models, before the first inference asks.
    ///
    /// Bounded and never fatal. A provider that cannot reach its API keeps its
    /// built-in table, which is exactly the behaviour that existed before any
    /// of this, so the worst case is the old answer rather than a daemon that
    /// will not start. `timeout` covers each provider separately: this runs on
    /// the start-up path, and an unreachable endpoint must cost a bounded wait
    /// rather than however long a connect takes to give up.
    ///
    /// Script providers are consulted only when `also` names one - in practice
    /// the machine's `default_provider`. `get` compiles them on demand, so
    /// priming the lot would compile every `.rhai` provider on disk whether or
    /// not a run touches one; priming the one a machine has actually chosen
    /// costs a single compile of a script that is about to be used anyway, and
    /// it is what lets that provider answer [`Provider::serves_model`] and so
    /// win an open route (issue #598).
    pub async fn prime_capabilities(&self, timeout: std::time::Duration, also: Option<&str>) {
        let mut targets: Vec<(String, Arc<dyn Provider>)> = self
            .providers
            .iter()
            .map(|(name, provider)| (name.clone(), provider.clone()))
            .collect();
        // Only when it is not already registered natively: a native provider of
        // the same name wins everywhere else, and priming it twice would be a
        // second network call for one answer.
        if let Some(name) = also
            && !self.providers.contains_key(name)
            && let Some(provider) = self.get(name)
        {
            targets.push((name.to_string(), provider));
        }
        for (name, provider) in targets {
            match tokio::time::timeout(timeout, provider.prime_capabilities()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(
                    provider = %name,
                    error = %e,
                    "could not read this provider's model list, so model sizes \
                     come from the table compiled into this build; a model it \
                     does not name gets a conservative window"
                ),
                Err(_) => tracing::warn!(
                    provider = %name,
                    timeout_secs = timeout.as_secs(),
                    "timed out reading this provider's model list, so model \
                     sizes come from the table compiled into this build"
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
    /// registered". Building the list on `provider_names` alone is what hid
    /// script providers from `lev models list` (#523) and then, in the same
    /// shape, from `GET /api/models` (#531).
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
    }

    struct StubProvider {
        primed: Arc<std::sync::atomic::AtomicUsize>,
        outcome: PrimeOutcome,
    }

    #[async_trait::async_trait]
    impl Provider for StubProvider {
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
    }

    /// What an *enumeration* needs, and what `provider_names` cannot give it:
    /// the script providers too. Building `GET /api/models` and
    /// `lev models list --remote` on `provider_names` is what hid them
    /// (issues #523, #531).
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
        Arc::new(StubProvider {
            primed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            outcome: PrimeOutcome::Ok,
        })
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
                outcome,
            }),
            primed,
        )
    }

    #[tokio::test]
    async fn priming_reaches_every_registered_provider() {
        let mut reg = ProviderRegistry::new();
        let (p, primed) = priming(PrimeOutcome::Ok);
        reg.register("prime".to_string(), p);
        reg.register("other".to_string(), mock());

        reg.prime_capabilities(std::time::Duration::from_secs(5), None)
            .await;
        assert_eq!(primed.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// A provider that cannot answer is a warning, not a failure: the daemon
    /// has to start whether or not an API is reachable.
    #[tokio::test]
    async fn a_failing_prime_does_not_stop_the_rest() {
        let mut reg = ProviderRegistry::new();
        let (bad, bad_calls) = priming(PrimeOutcome::Fails);
        reg.register("bad".to_string(), bad);
        reg.prime_capabilities(std::time::Duration::from_secs(5), None)
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
        reg.prime_capabilities(std::time::Duration::from_secs(10), None)
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
    /// it can answer what it serves on the synchronous resolve path (#598).
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

        reg.prime_capabilities(std::time::Duration::from_secs(5), Some("spark"))
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

        reg.prime_capabilities(std::time::Duration::from_secs(5), Some("prime"))
            .await;
        assert_eq!(primed.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// Naming something that resolves to nothing is not an error - a machine
    /// may name a default it has not set up yet.
    #[tokio::test]
    async fn naming_a_provider_that_does_not_resolve_is_harmless() {
        let reg = ProviderRegistry::new();
        reg.prime_capabilities(std::time::Duration::from_secs(5), Some("nope"))
            .await;
    }

    #[tokio::test]
    async fn stub_provider_methods_are_exercised() {
        let p = StubProvider {
            primed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            outcome: PrimeOutcome::Ok,
        };
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
}
