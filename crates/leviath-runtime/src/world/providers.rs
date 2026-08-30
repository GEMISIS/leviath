//! Swapping the world's provider registry for one built from a newer config.
//!
//! The daemon builds its registry from `config.toml` once at boot and holds
//! it as the [`Providers`] resource. A user who then ran `lev setup` to move
//! to another provider, or removed a key, found every new run still resolving
//! against the boot-time set: the file had changed, the registry had not, and
//! nothing short of a daemon restart rebuilt it. [`PipelineWorld::replace_providers`]
//! is the seam that rebuild goes through.

use super::*;
use crate::pipeline::ProviderCircuits;

impl PipelineWorld {
    /// Install `registry` as the world's providers, retiring rather than
    /// dropping whatever the old one had that the new one lacks (see
    /// [`ProviderRegistry::retiring_from`]), and forget the circuit-breaker
    /// record of every provider named in `credentials_changed`: the failures
    /// that opened that circuit belong to the old key.
    ///
    /// A run that is mid-stage keeps its `provider_name`, so it finishes the
    /// stage on the provider it started on (through the retired entry when
    /// the provider is gone from the config); only resolution for a new run,
    /// a new stage or a reloaded run sees the new set.
    pub fn replace_providers(
        &mut self,
        registry: ProviderRegistry,
        credentials_changed: &[String],
    ) {
        // Every `PipelineWorld` has the resource from construction, so there
        // is always a previous set to retire from.
        let registry = registry.retiring_from(self.providers());
        if !credentials_changed.is_empty()
            && let Some(mut circuits) = self.world.get_resource_mut::<ProviderCircuits>()
        {
            for name in credentials_changed {
                circuits.forget(name);
            }
        }
        tracing::info!(
            providers = ?registry.resolvable_names(),
            retired = ?registry.retired_names(),
            credentials_changed = ?credentials_changed,
            "provider registry rebuilt from the current config"
        );
        self.world.insert_resource(Providers(registry));
        self.wake.notify_one();
    }

    /// The world's current provider registry.
    pub fn providers(&self) -> &ProviderRegistry {
        &self
            .world
            .get_resource::<Providers>()
            .expect("Providers resource present in a PipelineWorld")
            .0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::CircuitPolicy;
    use crate::tool_bridge::BoxedToolExec;
    use leviath_providers::{
        InferenceRequest, InferenceResponse, ModelCapabilities, Provider, UnavailableReason,
    };

    struct Named(&'static str);

    #[async_trait::async_trait]
    impl Provider for Named {
        async fn infer(
            &self,
            _req: &InferenceRequest,
        ) -> leviath_providers::Result<InferenceResponse> {
            Err(ProviderError::Other("never called".to_string()))
        }
        async fn count_tokens(&self, _t: &str, _m: &str) -> usize {
            1
        }
        fn max_context_tokens(&self, _m: &str) -> usize {
            1000
        }
        fn name(&self) -> &str {
            self.0
        }
        fn capabilities(&self, _m: &str) -> ModelCapabilities {
            ModelCapabilities::default()
        }
    }

    struct NoTools;
    impl ToolService for NoTools {
        fn exec_for(
            &self,
            _entity: Entity,
            _calls: Vec<leviath_providers::ToolCall>,
            _progress: crate::pipeline::ToolProgress,
        ) -> BoxedToolExec {
            Box::new(move || Box::pin(async move { Vec::new() }))
        }
    }

    fn registry_of(names: &[&'static str]) -> ProviderRegistry {
        let mut r = ProviderRegistry::new();
        for n in names {
            r.register(n.to_string(), Arc::new(Named(n)));
        }
        r
    }

    fn world_with(names: &[&'static str], circuits: bool) -> PipelineWorld {
        let mut world = PipelineWorld::new(
            registry_of(names),
            Arc::new(NoTools),
            InferencePoolConfig::new(),
            1,
            None,
            Handle::current(),
        );
        if circuits {
            world.world_mut().init_resource::<ProviderCircuits>();
        }
        world
    }

    #[tokio::test]
    async fn a_rebuilt_registry_replaces_the_resource_and_retires_what_it_lost() {
        let mut world = world_with(&["anthropic"], true);
        world.replace_providers(registry_of(&["openrouter"]), &[]);
        let providers = world.providers();
        assert!(providers.has("openrouter"), "the new provider resolves");
        assert!(
            !providers.has("anthropic"),
            "the removed one no longer resolves"
        );
        assert!(
            providers.get("anthropic").is_some(),
            "but a run mid-stage on it can still call it"
        );
        assert_eq!(providers.retired_names(), vec!["anthropic".to_string()]);
    }

    #[tokio::test]
    async fn a_provider_registered_again_is_live_not_retired() {
        let mut world = world_with(&["anthropic"], true);
        world.replace_providers(registry_of(&["openrouter"]), &[]);
        world.replace_providers(registry_of(&["anthropic"]), &[]);
        let providers = world.providers();
        assert!(providers.has("anthropic"));
        assert_eq!(providers.retired_names(), vec!["openrouter".to_string()]);
    }

    #[tokio::test]
    async fn a_changed_credential_clears_that_providers_circuit_and_no_other() {
        let mut world = world_with(&["anthropic", "openai"], true);
        let policy = CircuitPolicy {
            failures_before_open: 1,
            cooldown_secs: 300,
        };
        {
            let mut circuits = world.world_mut().resource_mut::<ProviderCircuits>();
            circuits.record_failure(
                "anthropic",
                UnavailableReason::CreditsExhausted,
                None,
                0,
                &policy,
            );
            circuits.record_failure("openai", UnavailableReason::AuthFailed, None, 0, &policy);
        }
        world.replace_providers(
            registry_of(&["anthropic", "openai"]),
            &["anthropic".to_string()],
        );
        let circuits = world.world().resource::<ProviderCircuits>();
        assert!(
            !circuits.is_open("anthropic", 1, &policy),
            "the new key starts with a clean record"
        );
        assert!(
            circuits.is_open("openai", 1, &policy),
            "an untouched provider keeps its record"
        );
    }

    #[tokio::test]
    async fn replacing_with_no_circuits_resource_still_installs_the_registry() {
        // An embedded world that never installed the breaker: a credential
        // change has no record to forget, and must not panic looking for one.
        let mut world = world_with(&["a"], false);
        world.replace_providers(registry_of(&["b"]), &["a".to_string()]);
        assert!(world.providers().has("b"));
    }

    #[tokio::test]
    async fn the_stand_in_tool_service_answers_nothing() {
        // The world needs a tool service and these tests never call a tool, so
        // this one exists to be handed over. Driving it once keeps it honest
        // rather than untested.
        let exec = NoTools.exec_for(
            Entity::PLACEHOLDER,
            Vec::new(),
            crate::pipeline::noop_progress(),
        );
        assert!(exec().await.is_empty());
    }

    /// The stand-in provider answers like a provider, so the registry it is
    /// registered in behaves like a real one (and the trait methods this file
    /// leans on are not untested code).
    #[tokio::test]
    async fn the_stand_in_provider_answers_every_call() {
        let world = world_with(&["a"], false);
        let provider = world.providers().get("a").unwrap();
        assert_eq!(provider.name(), "a");
        assert_eq!(provider.max_context_tokens("m"), 1000);
        assert_eq!(provider.count_tokens("hello", "m").await, 1);
        assert_eq!(provider.capabilities("m").max_context_tokens, 8192);
        let request = InferenceRequest {
            system: Vec::new(),
            messages: Vec::new(),
            model: "m".to_string(),
            max_tokens: 16,
            temperature: 0.0,
            tools: Vec::new(),
            extra: Default::default(),
            request_timeout_secs: None,
        };
        assert!(provider.infer(&request).await.is_err());
    }

    #[tokio::test]
    async fn a_script_layer_is_carried_across_a_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let layer = Arc::new(crate::script_provider::ScriptProviderLayer::new(
            dir.path().to_path_buf(),
            Default::default(),
            Default::default(),
            None,
            Vec::new(),
        ));
        let mut world = world_with(&["a"], false);
        world.replace_providers(registry_of(&["b"]).with_script_layer(layer), &[]);
        assert!(
            world.providers().script_layer().is_some(),
            "the rebuilt registry keeps loading .rhai providers"
        );
    }
}
